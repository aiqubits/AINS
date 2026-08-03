//! 后台任务（AINS_PLAN 7+.4）：`BackgroundTaskManager` + `background_task` 工具。
//!
//! Native 先行：后台任务本质是子进程/长时运行，依赖 `tokio::process` +
//! `tokio::spawn`，故本模块仅在非 wasm 目标编译（Web 端无子进程模型）。
//! 对齐 OpenHarness `tasks/`（`BackgroundTaskManager` + `TaskRecord`/`TaskStatus`
//! + `task_run`/`stop`/`list`/`update`/`output` 面）。
//!
//! 任务状态机：Running →（完成）Completed/Failed；`stop` 抢占 → Killed。
//! 完成回写守卫：已 Killed 的任务不被完成回调覆盖（对齐基线）。输出按
//! `TASK_OUTPUT_MAX_BYTES` 有界读取，防无界输出耗尽内存。
//!
//! **安全边界**：任务命令必经 [`Sandbox`] 执行（与 `shell_command` 工具同源
//! 关口），`capabilities().shell == false` 时拒绝启动（fail-closed，不降级
//! 直跑）；`stop` 经 `ShellRequest.cancel` 协作式取消，由沙箱后端终止整个
//! 进程树（Unix killpg / Windows kill-on-close），不使用 `abort`——abort 只
//! 杀包装进程，沙箱内命令树会残留。
//!
//! **接线前提（review）**：`TaskRecord` 无 owner/ACL，任务 id 顺序可预测
//! （`task-{seq}`），`list`/`show`/`output` 按只读自动放行——单 agent 进程
//! 内使用是安全的，但**在 swarm 子代理共享同一 `BackgroundTaskManager` 之前**，
//! 必须为任务增加归属校验（spawner 身份），否则任一被授予 `background_task`
//! 的子代理可枚举并读取/干扰其他 agent（含 lead）的任务。

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::ToolError;
use crate::memory::now_ms;
use crate::policy::{
    NoopSandbox, Sandbox, ShellOutputSink, ShellRequest, resolve_shell_cwd_within_workspace,
};
use crate::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

/// 单任务 stdout/stderr 合计输出上限（防无界输出耗尽宿主内存）。
const TASK_OUTPUT_MAX_BYTES: usize = 256 * 1024;

/// 同时运行的后台任务上限。每个任务都可能占用一个子进程及其输出缓冲，因而
/// 不能只限制单任务输出，还必须限制并发任务数。
const MAX_ACTIVE_TASKS: usize = 16;

/// 保留的任务记录上限。仅淘汰最早的终态记录，运行中的任务永不因回收被移除。
const MAX_TASK_RECORDS: usize = 256;

/// Cap final backend output as well as streamed preview output. Custom
/// `Sandbox` implementations are not required to honour `max_output_bytes`.
/// Truncating at a character boundary keeps `TaskRecord::output` valid UTF-8.
fn cap_task_output(mut output: String) -> String {
    if output.len() > TASK_OUTPUT_MAX_BYTES {
        let mut end = TASK_OUTPUT_MAX_BYTES;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
    }
    output
}

/// 后台任务执行的异常兑底超时（7 天）。后台任务原则上由 `stop` 取消；
/// 超时仅防止沙箱实现异常时任务无限挂起。
const TASK_RUNTIME_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);
/// stop 等待任务收尾的宽限期：自定义沙箱若不响应协作式取消（忽略
/// `cancel` 标志、不终止进程树），stop 在宽限期后超时返回，避免无限挂起
/// （任务保持 Running，可稍后重试；review 修复：历史实现无限等待）。
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(30);

/// 后台任务状态（对齐基线 `TaskStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Killed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
        }
    }
}

/// 任务记录（对齐基线 `TaskRecord` + metadata 展开为字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    /// 任务类型（当前 `"shell"`）。
    pub kind: String,
    pub description: String,
    pub status: TaskStatus,
    /// 进度百分比 0..=100（由 agent 通过 update 汇报）。
    pub progress: u8,
    pub status_note: Option<String>,
    /// 已捕获输出（有界）。
    pub output: String,
    /// 退出码（终止/被杀时可能为 None）。
    pub exit_code: Option<i32>,
    pub created_at: i64,
}

impl TaskRecord {
    /// 单行摘要（列表渲染）。
    pub fn summary(&self) -> String {
        format!(
            "{} [{}] {}% {}",
            self.id,
            self.status.as_str(),
            self.progress,
            self.description
        )
    }
}

struct State {
    tasks: HashMap<String, TaskRecord>,
    order: Vec<String>,
    handles: HashMap<String, JoinHandle<()>>,
    /// Completion notification shared by concurrent wait/stop callers.
    done: HashMap<String, Arc<Notify>>,
    /// 任务取消标志（stop 置位 → 沙箱后端终止进程树）。
    cancels: HashMap<String, Arc<AtomicBool>>,
    seq: u64,
}

/// 将任务历史保持在有界范围内。调用方持有 `State` 锁；只移除终态记录，
/// 以免 list/get/stop 对活跃任务失去引用。
fn prune_terminal_tasks(state: &mut State) {
    while state.tasks.len() > MAX_TASK_RECORDS {
        let Some(index) = state.order.iter().position(|id| {
            state
                .tasks
                .get(id)
                .is_some_and(|task| task.status.is_terminal())
        }) else {
            // 理论上不会发生（活跃任务也受上限约束），但绝不为了满足历史
            // 上限而淘汰一个仍在运行的任务。
            break;
        };
        let id = state.order.remove(index);
        state.tasks.remove(&id);
        state.handles.remove(&id);
        state.done.remove(&id);
        state.cancels.remove(&id);
    }
}

/// 后台任务管理器（Native）：经 [`Sandbox`] 执行后台任务，跟踪状态/输出/进度。
/// 线程安全（`Arc<Mutex<..>>`），可被多处克隆共享。
#[derive(Clone)]
pub struct BackgroundTaskManager {
    state: Arc<Mutex<State>>,
    /// 任务命令执行的强制关口（与 `shell_command` 同一沙箱）。
    sandbox: Arc<dyn Sandbox>,
    /// 后台 shell 的唯一可写工作区。未显式配置时拒绝执行，避免公共管理器
    /// API 被传入任意宿主目录后授予沙箱可写绑定。
    workspace: Option<PathBuf>,
    /// stop 等待任务收尾的宽限期（可配置；测试用短值触发超时路径）。
    stop_grace: Duration,
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskManager {
    /// 无沙箱构造（fail-closed）：不注入沙箱时所有任务被拒绝——与项目
    /// 默认拒绝原则一致，绝不降级为宿主直跑。
    pub fn new() -> Self {
        Self::with_sandbox(Arc::new(NoopSandbox))
    }

    /// 显式注入沙箱但不授予工作区。此构造仅适用于只读管理操作；任何
    /// `spawn_shell` 都会 fail-closed。宿主应使用 [`Self::with_sandbox_in_workspace`]
    /// 来启用任务执行。
    pub fn with_sandbox(sandbox: Arc<dyn Sandbox>) -> Self {
        Self::with_sandbox_in_optional_workspace(sandbox, None)
    }

    /// 显式注入沙箱与可写工作区。每次启动都会重新 canonicalize 并验证 cwd
    /// 位于该工作区内（含符号链接检查），与 `shell_command` 保持同一边界。
    pub fn with_sandbox_in_workspace(
        sandbox: Arc<dyn Sandbox>,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        Self::with_sandbox_in_optional_workspace(sandbox, Some(workspace.into()))
    }

    fn with_sandbox_in_optional_workspace(
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<PathBuf>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                tasks: HashMap::new(),
                order: Vec::new(),
                handles: HashMap::new(),
                done: HashMap::new(),
                cancels: HashMap::new(),
                seq: 0,
            })),
            sandbox,
            workspace,
            stop_grace: STOP_GRACE_PERIOD,
        }
    }

    /// 自定义 stop 宽限期（测试与特殊宿主：默认 30s）。
    pub fn with_stop_grace_period(mut self, grace: Duration) -> Self {
        self.stop_grace = grace;
        self
    }

    /// 以 shell 后台任务方式运行 `command`；立即返回 task_id，进程在沙箱内
    /// 后台执行。`cwd` 与 `shell_command` 工具同口径（工具调用传入
    /// `ToolContext.cwd`，避免与 shell 工具工作目录分叉）。沙箱不提供
    /// shell 能力时拒绝启动（fail-closed）。
    pub async fn spawn_shell(
        &self,
        description: &str,
        command: &str,
        cwd: &Path,
    ) -> Result<String, ToolError> {
        if command.trim().is_empty() {
            return Err(ToolError::InvalidInput("empty command".into()));
        }
        // 沙箱关口：与 shell_command 工具同源——无 shell 能力即拒绝，
        // 绝不降级为宿主直跑（B1 修复：历史实现直接 `sh -c` 绕过沙箱）。
        if !self.sandbox.capabilities().shell {
            return Err(ToolError::PermissionDenied(format!(
                "sandbox '{}' does not provide shell execution; background task refused \
                 (fail-closed: no un-sandboxed shell)",
                self.sandbox.name()
            )));
        }
        let workspace = self.workspace.as_deref().ok_or_else(|| {
            ToolError::PermissionDenied(
                "background tasks require an explicitly configured workspace; refusing to bind an arbitrary host cwd"
                    .into(),
            )
        })?;
        let cwd = resolve_shell_cwd_within_workspace(cwd, workspace).map_err(|reason| {
            ToolError::InvalidInput(format!(
                "background task cwd {} is outside the workspace: {reason}",
                cwd.display()
            ))
        })?;
        // 仅在这里分配 ID。配额检查必须与下方的记录登记处于同一个锁区间，
        // 否则并发调用可能都在登记前观察到尚有容量。
        let id = {
            let mut st = self.state.lock().await;
            st.seq += 1;
            format!("task-{}", st.seq)
        };

        let state = Arc::clone(&self.state);
        let sandbox = Arc::clone(&self.sandbox);
        let command = command.to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_stop = Arc::clone(&cancel);
        let task_id = id.clone();
        let done = Arc::new(Notify::new());
        let streamed_bytes = Arc::new(AtomicUsize::new(0));
        let output_state = Arc::clone(&state);
        let output_task_id = id.clone();
        let output_bytes_for_sink = Arc::clone(&streamed_bytes);
        let output_sink = ShellOutputSink::new(move |chunk| {
            // The backend already applies this cap.  Enforce it again here so
            // a custom Sandbox implementation cannot make task state unbounded.
            let accepted_len = loop {
                let used = output_bytes_for_sink.load(Ordering::Relaxed);
                if used >= TASK_OUTPUT_MAX_BYTES {
                    return;
                }
                let accepted_len = chunk.len().min(TASK_OUTPUT_MAX_BYTES - used);
                if output_bytes_for_sink
                    .compare_exchange_weak(
                        used,
                        used + accepted_len,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    break accepted_len;
                }
            };
            let accepted = &chunk[..accepted_len];
            let text = String::from_utf8_lossy(accepted).into_owned();
            let output_state = Arc::clone(&output_state);
            let output_task_id = output_task_id.clone();
            tokio::spawn(async move {
                if let Some(task) = output_state.lock().await.tasks.get_mut(&output_task_id)
                    && task.status == TaskStatus::Running
                {
                    task.output.push_str(&text);
                }
            });
        });

        // 锁内检查配额并插入 tasks + order + cancels。检查和登记不可分离，
        // 否则并发 spawn 可同时通过检查而越过上限。JoinHandle 依赖 spawn
        // 返回值，随后单独插入；cancels 先就位保证 stop 在任何间隙都能取到
        // 取消标志（T1 语义保持）。
        // （review 修复：历史实现先 tokio::spawn 后插入全部状态，多线程
        // runtime 下闭包可与主任务并发——立即失败的 sandbox 路径（如 spawn
        // 错误）会在插入前完成回写，记录因此永久 Running。先插记录后，闭包
        // 总能找到记录并回写终态。）
        {
            let mut st = self.state.lock().await;
            let active_tasks = st
                .tasks
                .values()
                .filter(|task| task.status == TaskStatus::Running)
                .count();
            if active_tasks >= MAX_ACTIVE_TASKS {
                return Err(ToolError::Execution(format!(
                    "background task limit reached ({MAX_ACTIVE_TASKS} active tasks)"
                )));
            }
            st.tasks.insert(
                id.clone(),
                TaskRecord {
                    id: id.clone(),
                    kind: "shell".into(),
                    description: description.to_string(),
                    status: TaskStatus::Running,
                    progress: 0,
                    status_note: None,
                    output: String::new(),
                    exit_code: None,
                    created_at: now_ms(),
                },
            );
            st.order.push(id.clone());
            st.cancels.insert(id.clone(), cancel_for_stop);
            st.done.insert(id.clone(), Arc::clone(&done));
            prune_terminal_tasks(&mut st);
        }

        let handle = tokio::spawn(async move {
            let outcome = sandbox
                .exec_shell(ShellRequest {
                    command,
                    cwd,
                    // 后台任务原则上不设硬性时限（由 stop 取消）；超时仅作
                    // 沙箱实现异常的兜底。
                    timeout: TASK_RUNTIME_TIMEOUT,
                    max_output_bytes: TASK_OUTPUT_MAX_BYTES,
                    cancel: Some(Arc::clone(&cancel)),
                    output_sink: Some(output_sink),
                })
                .await;
            let mut st = state.lock().await;
            if let Some(task) = st.tasks.get_mut(&task_id) {
                // 完成回写守卫：被 stop 抢占（Killed）的任务不被覆盖。
                if task.status != TaskStatus::Killed {
                    match outcome {
                        Ok(outcome) => {
                            task.exit_code = outcome.exit_code;
                            // Replace the incremental preview with the backend's canonical
                            // final merge (stdout/stderr ordering + all bytes). Any sink
                            // callback queued after terminal state is ignored above.
                            task.output = cap_task_output(outcome.output);
                            task.status = if outcome.cancelled {
                                // 协作式取消（stop 置位）→ Killed。后端可能在
                                // 超时分支之外返回 cancelled，因此该标志优先级最高。
                                TaskStatus::Killed
                            } else if outcome.timed_out {
                                task.status_note = Some("hit the runtime timeout guard".into());
                                TaskStatus::Failed
                            } else if outcome.exit_code == Some(0) {
                                TaskStatus::Completed
                            } else {
                                TaskStatus::Failed
                            };
                        }
                        Err(e) => {
                            task.status = TaskStatus::Failed;
                            task.status_note = Some(format!("sandbox execution failed: {e}"));
                        }
                    }
                }
                // 任务已结束，清理 handle 与取消标志防止长期运行的内存累积。
                st.handles.remove(&task_id);
                st.cancels.remove(&task_id);
            }
            done.notify_waiters();
        });

        // 锁内插入 handle：任务若已终态（竞态窗口内闭包已先行完成回写，
        // 且闭包回写时 handle 尚未插入、未能清理）则跳过插入，避免已完成
        // 的 JoinHandle 在状态表中残留。
        {
            let mut st = self.state.lock().await;
            let terminal = st
                .tasks
                .get(&id)
                .is_some_and(|task| task.status.is_terminal());
            if !terminal {
                st.handles.insert(id.clone(), handle);
            }
        }
        Ok(id)
    }

    /// 全部任务（注册顺序）。
    pub async fn list(&self) -> Vec<TaskRecord> {
        let st = self.state.lock().await;
        st.order
            .iter()
            .filter_map(|id| st.tasks.get(id).cloned())
            .collect()
    }

    /// 按 id 取任务记录。
    pub async fn get(&self, id: &str) -> Option<TaskRecord> {
        self.state.lock().await.tasks.get(id).cloned()
    }

    /// 等待任务结束并返回最终记录（测试/同步等待用）。
    ///
    pub async fn wait(&self, id: &str) -> Result<TaskRecord, ToolError> {
        let done = {
            self.state
                .lock()
                .await
                .done
                .get(id)
                .cloned()
                .ok_or_else(|| ToolError::NotFound(id.to_string()))?
        };
        loop {
            // 必须先把 future 注册为 waiter，再读取终态：`notify_waiters()`
            // 不保留 permit，若完成回调恰好落在“检查 Running”与
            // `notified.await` 之间，未 enable 的 future 不会被唤醒，wait 会
            // 永久挂起。Tokio 的 enable 模式消除此 lost-wakeup 窗口。
            let notified = done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.state.lock().await.tasks.get(id).cloned() {
                Some(task) if task.status.is_terminal() => return Ok(task),
                Some(_) => {}
                // 历史记录可在另一个 spawn 中被保留策略淘汰；不能在已经
                // 收到通知后继续等待一个永远不会再出现的 id。
                None => return Err(ToolError::NotFound(id.to_string())),
            }
            notified.await;
        }
    }

    /// 汇报进度 / 状态备注。
    pub async fn update(
        &self,
        id: &str,
        progress: Option<u8>,
        note: Option<String>,
    ) -> Result<TaskRecord, ToolError> {
        let mut st = self.state.lock().await;
        let task = st
            .tasks
            .get_mut(id)
            .ok_or_else(|| ToolError::NotFound(id.to_string()))?;
        // 终态（Completed/Failed/Killed）只读：任务已落定，progress/note
        // 不再有意义，且终态后写入会造成状态回写与展示不一致。
        if task.status.is_terminal() {
            return Err(ToolError::InvalidInput(format!(
                "task {id} is already {} and cannot be updated",
                task.status.as_str()
            )));
        }
        if let Some(p) = progress {
            task.progress = p.min(100);
        }
        if let Some(note) = note {
            let note = note.trim();
            task.status_note = if note.is_empty() {
                None
            } else {
                Some(note.to_string())
            };
        }
        Ok(task.clone())
    }

    /// 抢占停止任务：置协作式取消标志（沙箱后端终止整个进程树）并等待
    /// 任务收尾，然后置 Killed（终态任务原样返回）。
    ///
    /// 刻意不使用 `abort`：abort 只 drop 执行 future，`kill_on_drop` 仅杀沙箱
    /// 包装进程，沙箱内命令树会残留（B2/N4 同源缺陷）。
    pub async fn stop(&self, id: &str) -> Result<TaskRecord, ToolError> {
        // 锁内检查状态并取出共享取消/完成信号（不得持锁 await）。
        let cancel = {
            let st = self.state.lock().await;
            let status = st
                .tasks
                .get(id)
                .map(|t| t.status)
                .ok_or_else(|| ToolError::NotFound(id.to_string()))?;
            if status.is_terminal() {
                return Ok(st.tasks.get(id).cloned().expect("checked above"));
            }
            st.cancels.get(id).cloned()
        };
        if let Some(cancel) = cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        // 协作式等待：所有并发 stop/wait 调用共享同一个通知，任何调用方
        // 都不会在沙箱进程树真正收尾前返回。自定义 Sandbox 若忽略 cancel
        // 标志（进程树不终止），至多等待 stop_grace 后超时返回，避免无限
        // 挂起（review 修复）；任务保持 Running，可稍后重试 stop，Killed
        // 回写守卫不变。
        match tokio::time::timeout(self.stop_grace, self.wait(id)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(ToolError::Execution(
                    "task did not stop within the grace period; the sandbox backend \
                     did not respond to cooperative cancellation"
                        .into(),
                ));
            }
        }
        let mut st = self.state.lock().await;
        let task = st
            .tasks
            .get_mut(id)
            .ok_or_else(|| ToolError::NotFound(id.to_string()))?;
        // T2 修复：任务可能在检查后、await 前自然完成（Completed/Failed）——
        // 不覆盖终态，仅对仍 Running 的任务置 Killed。
        if !task.status.is_terminal() {
            task.status = TaskStatus::Killed;
        }
        Ok(task.clone())
    }

    /// 取任务输出。
    pub async fn output(&self, id: &str) -> Result<String, ToolError> {
        self.get(id)
            .await
            .map(|t| t.output)
            .ok_or_else(|| ToolError::NotFound(id.to_string()))
    }
}

/// `background_task` 工具：多路复用 task_run/list/show/stop/update/output
/// （对齐基线 `/tasks` 子命令面），仅 Desktop 原生注册。
pub struct BackgroundTaskTool {
    manager: Arc<BackgroundTaskManager>,
}

impl BackgroundTaskTool {
    pub fn new(manager: Arc<BackgroundTaskManager>) -> Self {
        Self { manager }
    }
}

fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

#[async_trait::async_trait]
impl Tool for BackgroundTaskTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "background_task".into(),
            description: "Run and manage long-running background tasks. \
                actions: run (spawn a shell command), list, show, stop, update (progress/note), output."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["run", "list", "show", "stop", "update", "output"]
                    },
                    "command": {"type": "string", "description": "shell command (action=run)"},
                    "description": {"type": "string", "description": "task description (action=run)"},
                    "task_id": {"type": "string", "description": "target task (show/stop/update/output)"},
                    "progress": {"type": "integer", "minimum": 0, "maximum": 100},
                    "note": {"type": "string"}
                },
                "required": ["action"]
            }),
        }
    }

    fn is_read_only(&self, input: &Value) -> bool {
        matches!(str_field(input, "action"), Some("list" | "show" | "output"))
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let action = str_field(&input, "action")
            .ok_or_else(|| ToolError::InvalidInput("missing 'action'".into()))?;
        let require_id = || -> Result<String, ToolError> {
            str_field(&input, "task_id")
                .map(str::to_string)
                .ok_or_else(|| ToolError::InvalidInput("missing 'task_id'".into()))
        };
        match action {
            "run" => {
                let command = str_field(&input, "command")
                    .ok_or_else(|| ToolError::InvalidInput("missing 'command'".into()))?;
                let description = str_field(&input, "description").unwrap_or(command);
                // cwd 与 shell_command 工具同口径（同一 ToolContext.cwd）。
                let id = self
                    .manager
                    .spawn_shell(description, command, ctx.cwd)
                    .await?;
                Ok(ToolResult::ok(format!("started background task {id}")))
            }
            "list" => {
                let tasks = self.manager.list().await;
                if tasks.is_empty() {
                    return Ok(ToolResult::ok("no background tasks"));
                }
                let body = tasks
                    .iter()
                    .map(TaskRecord::summary)
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolResult::ok(body))
            }
            "show" => {
                let id = require_id()?;
                let task = self
                    .manager
                    .get(&id)
                    .await
                    .ok_or_else(|| ToolError::NotFound(id.clone()))?;
                let json = serde_json::to_string_pretty(&task)
                    .map_err(|e| ToolError::Execution(e.to_string()))?;
                Ok(ToolResult::ok(json))
            }
            "stop" => {
                let id = require_id()?;
                let task = self.manager.stop(&id).await?;
                Ok(ToolResult::ok(format!(
                    "task {} is now {}",
                    task.id,
                    task.status.as_str()
                )))
            }
            "update" => {
                let id = require_id()?;
                let progress = input
                    .get("progress")
                    .and_then(Value::as_u64)
                    .map(|p| p.min(100) as u8);
                let note = str_field(&input, "note").map(str::to_string);
                let task = self.manager.update(&id, progress, note).await?;
                Ok(ToolResult::ok(task.summary()))
            }
            "output" => {
                let id = require_id()?;
                Ok(ToolResult::ok(self.manager.output(&id).await?))
            }
            other => Err(ToolError::InvalidInput(format!("unknown action: {other}"))),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::System
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use crate::policy::{SandboxCapabilities, SandboxError, ShellOutcome};
    use std::path::PathBuf;

    /// 测试桩沙箱：无隔离直跑 sh（仅测试用；生产必须经真实沙箱，真实
    /// 进程树终止由 sandbox_linux / sandbox_windows 的集成测试覆盖）。
    /// 尊重协作式取消：置位后返回 timed_out（模拟真实后端终止）。
    struct PassthroughSandbox;

    #[async_trait::async_trait]
    impl Sandbox for PassthroughSandbox {
        fn name(&self) -> &'static str {
            "test-passthrough"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                network_policy: false,
                filesystem_policy: false,
            }
        }

        async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            use std::process::Stdio;
            let cancel = request.cancel.clone();
            let run = async {
                let output = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&request.command)
                    .current_dir(&request.cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .await
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                // 与真实后端一致：stdout + stderr 合并捕获。
                let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
                let err_str = String::from_utf8_lossy(&output.stderr);
                if !err_str.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&err_str);
                }
                Ok::<_, SandboxError>((combined, output.status.code()))
            };
            let cancel_fut = crate::policy::sandbox::wait_cancel(cancel);
            tokio::pin!(cancel_fut);
            let result = tokio::select! {
                res = run => res,
                _ = &mut cancel_fut => {
                    return Ok(ShellOutcome {
                        output: String::new(),
                        exit_code: None,
                        timed_out: true,
                        cancelled: true,
                    });
                }
            };
            let (output, exit_code) = result?;
            Ok(ShellOutcome {
                output,
                exit_code,
                timed_out: false,
                cancelled: false,
            })
        }
    }

    fn manager() -> BackgroundTaskManager {
        BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(PassthroughSandbox),
            PathBuf::from("/tmp"),
        )
    }

    /// 立即失败沙箱：capabilities 声称可用，但 exec_shell 同步返回错误——
    /// 专测 spawn/插入竞态（多线程 runtime 下闭包可与主任务并发完成）。
    struct FailFastSandbox;

    #[async_trait::async_trait]
    impl Sandbox for FailFastSandbox {
        fn name(&self) -> &'static str {
            "test-fail-fast"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                network_policy: false,
                filesystem_policy: false,
            }
        }

        async fn exec_shell(&self, _request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            Err(SandboxError::Execution("boom".into()))
        }
    }

    /// 忽略协作式取消的沙箱：进程照常运行到自身结束（模拟自定义后端
    /// 不实现 cancel 契约）；专测 stop 的宽限期超时路径。
    struct IgnoreCancelSandbox;

    #[async_trait::async_trait]
    impl Sandbox for IgnoreCancelSandbox {
        fn name(&self) -> &'static str {
            "test-ignore-cancel"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                network_policy: false,
                filesystem_policy: false,
            }
        }

        async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            // 刻意不消费 request.cancel：进程跑满 timeout（由 spawn 侧传入）。
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&request.command)
                .current_dir(&request.cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| SandboxError::Execution(e.to_string()))?;
            Ok(ShellOutcome {
                output: String::new(),
                exit_code: output.status.code(),
                timed_out: false,
                cancelled: false,
            })
        }
    }

    /// 仅在收到 `stop` 的协作式取消后结束，用于占住活跃任务配额。
    struct CancelWaitSandbox;

    #[async_trait::async_trait]
    impl Sandbox for CancelWaitSandbox {
        fn name(&self) -> &'static str {
            "test-cancel-wait"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                ..Default::default()
            }
        }

        async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            crate::policy::sandbox::wait_cancel(request.cancel).await;
            Ok(ShellOutcome {
                output: String::new(),
                exit_code: None,
                timed_out: true,
                cancelled: true,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn immediate_failure_tasks_never_stuck_running_and_handles_cleaned() {
        // review 修复回归：历史实现先 spawn 后插入状态，多线程 runtime 下
        // 立即失败的沙箱路径可能先于插入完成回写 → 记录永久 Running +
        // 已完成 JoinHandle 残留。修复后：记录先插入（闭包总能回写终态），
        // handle 终态跳过插入（不残留）。
        let mgr = BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(FailFastSandbox),
            PathBuf::from("/tmp"),
        );
        for i in 0..MAX_ACTIVE_TASKS {
            let _ = mgr
                .spawn_shell("fail", "should-not-run", Path::new("/tmp"))
                .await
                .unwrap();
            // 并发 stop 抢占任意任务，锻炼 stop/handle-插入交错路径。
            if i % 4 == 0 {
                let id = format!("task-{}", i + 1);
                let _ = mgr.stop(&id).await;
            }
        }
        // 等待全部任务落定终态（多线程下闭包回写与主任务插入交错）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let st = mgr.state.lock().await;
            let all_terminal = st.tasks.values().all(|t| t.status.is_terminal());
            if all_terminal || tokio::time::Instant::now() > deadline {
                break;
            }
            drop(st);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let st = mgr.state.lock().await;
        for (id, task) in &st.tasks {
            assert!(
                task.status.is_terminal(),
                "task {id} stuck in {:?}",
                task.status
            );
        }
        // 无残留：失败任务立即结束，handle/cancel 应已被回写路径清理或未插入。
        assert!(
            st.handles.is_empty(),
            "no JoinHandle should remain: {:?}",
            st.handles.keys()
        );
        assert!(
            st.cancels.is_empty(),
            "no cancel flag should remain: {:?}",
            st.cancels.keys()
        );
    }

    #[tokio::test]
    async fn active_task_limit_rejects_excess_and_keeps_running_tasks_manageable() {
        let mgr = BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(CancelWaitSandbox),
            PathBuf::from("/tmp"),
        );
        let mut ids = Vec::new();
        for _ in 0..MAX_ACTIVE_TASKS {
            ids.push(
                mgr.spawn_shell("held", "ignored", Path::new("/tmp"))
                    .await
                    .unwrap(),
            );
        }

        let error = mgr
            .spawn_shell("one too many", "ignored", Path::new("/tmp"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::Execution(message) if message.contains("limit reached"))
        );
        assert_eq!(mgr.list().await.len(), MAX_ACTIVE_TASKS);

        for id in ids {
            assert_eq!(mgr.stop(&id).await.unwrap().status, TaskStatus::Killed);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_spawns_cannot_bypass_active_task_limit() {
        let mgr = Arc::new(BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(CancelWaitSandbox),
            PathBuf::from("/tmp"),
        ));
        let attempts = MAX_ACTIVE_TASKS + 8;
        let barrier = Arc::new(tokio::sync::Barrier::new(attempts));
        let mut spawns = Vec::with_capacity(attempts);

        for index in 0..attempts {
            let mgr = Arc::clone(&mgr);
            let barrier = Arc::clone(&barrier);
            spawns.push(tokio::spawn(async move {
                barrier.wait().await;
                mgr.spawn_shell(&format!("held-{index}"), "ignored", Path::new("/tmp"))
                    .await
            }));
        }

        let mut ids = Vec::new();
        let mut rejected = 0;
        for spawn in spawns {
            match spawn.await.unwrap() {
                Ok(id) => ids.push(id),
                Err(ToolError::Execution(message)) if message.contains("limit reached") => {
                    rejected += 1;
                }
                Err(error) => panic!("unexpected spawn error: {error}"),
            }
        }

        assert_eq!(ids.len(), MAX_ACTIVE_TASKS);
        assert_eq!(rejected, attempts - MAX_ACTIVE_TASKS);
        assert_eq!(mgr.list().await.len(), MAX_ACTIVE_TASKS);
        for id in ids {
            assert_eq!(mgr.stop(&id).await.unwrap().status, TaskStatus::Killed);
        }
    }

    #[tokio::test]
    async fn task_record_retention_prunes_oldest_terminal_records() {
        let mgr = BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(FailFastSandbox),
            PathBuf::from("/tmp"),
        );
        {
            let mut state = mgr.state.lock().await;
            for index in 0..MAX_TASK_RECORDS {
                let id = format!("finished-{index}");
                state.order.push(id.clone());
                state.done.insert(id.clone(), Arc::new(Notify::new()));
                state.tasks.insert(
                    id.clone(),
                    TaskRecord {
                        id,
                        kind: "shell".into(),
                        description: "finished".into(),
                        status: TaskStatus::Completed,
                        progress: 100,
                        status_note: None,
                        output: String::new(),
                        exit_code: Some(0),
                        created_at: now_ms(),
                    },
                );
            }
        }

        let id = mgr
            .spawn_shell("new", "ignored", Path::new("/tmp"))
            .await
            .unwrap();
        let _ = mgr.wait(&id).await.unwrap();

        assert!(mgr.get("finished-0").await.is_none());
        assert_eq!(mgr.list().await.len(), MAX_TASK_RECORDS);
        assert!(matches!(
            mgr.wait("finished-0").await,
            Err(ToolError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn noop_sandbox_refuses_spawn_fail_closed() {
        // B1 回归：无沙箱（或沙箱无 shell 能力）时后台任务必须被拒绝，
        // 绝不降级为宿主直跑。
        let mgr = BackgroundTaskManager::new();
        let err = mgr
            .spawn_shell("x", "echo hi", Path::new("/tmp"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied(_)),
            "expected PermissionDenied, got {err:?}"
        );
        assert!(
            mgr.list().await.is_empty(),
            "no task record should be created"
        );
    }

    #[tokio::test]
    async fn spawn_shell_rejects_cwd_outside_workspace() {
        // Review regression: the manager owns the same canonical workspace
        // boundary as shell_command; relative/root/outside paths cannot turn
        // into arbitrary writable sandbox binds.
        let mgr = manager();
        assert!(matches!(
            mgr.spawn_shell("rel", "echo hi", Path::new(".")).await,
            Err(ToolError::InvalidInput(_))
        ));
        assert!(matches!(
            mgr.spawn_shell("rel2", "echo hi", Path::new("sub/dir"))
                .await,
            Err(ToolError::InvalidInput(_))
        ));
        assert!(matches!(
            mgr.spawn_shell("root", "echo hi", Path::new("/")).await,
            Err(ToolError::InvalidInput(_))
        ));
        // 被拒绝的请求不产生任务记录（与 empty command 同口径）。
        assert!(mgr.list().await.is_empty());
        // Workspace cwd succeeds.
        let id = mgr
            .spawn_shell("ok", "echo ok", Path::new("/tmp"))
            .await
            .unwrap();
        let rec = mgr.wait(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn direct_manager_without_workspace_fails_closed() {
        let mgr = BackgroundTaskManager::with_sandbox(Arc::new(PassthroughSandbox));
        let error = mgr
            .spawn_shell("outside", "echo unsafe", Path::new("/tmp"))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn stop_uses_cooperative_cancel_and_marks_killed() {
        // stop 经协作式取消（ShellRequest.cancel）终止任务并置 Killed；
        // 桩的 exec_shell 在 cancel 后返回 timed_out，任务回写被守卫跳过。
        let mgr = manager();
        let id = mgr
            .spawn_shell("sleeper", "sleep 30", Path::new("/tmp"))
            .await
            .unwrap();
        let rec = mgr.stop(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Killed);
        // 再次 stop 幂等（终态原样返回）
        assert_eq!(mgr.stop(&id).await.unwrap().status, TaskStatus::Killed);
    }

    #[tokio::test]
    async fn shell_task_completes_with_output() {
        let mgr = manager();
        let id = mgr
            .spawn_shell("echo test", "echo hello-bg", Path::new("/tmp"))
            .await
            .unwrap();
        let rec = mgr.wait(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Completed);
        assert_eq!(rec.exit_code, Some(0));
        assert!(rec.output.contains("hello-bg"));
    }

    struct StreamingSandbox {
        emitted_first: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl Sandbox for StreamingSandbox {
        fn name(&self) -> &'static str {
            "test-streaming"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                ..Default::default()
            }
        }

        async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            let sink = request
                .output_sink
                .expect("background tasks install an output sink");
            sink.push(b"first chunk\\n");
            self.emitted_first.notify_waiters();
            self.release.notified().await;
            sink.push(b"second chunk\\n");
            Ok(ShellOutcome {
                output: "first chunk\\nsecond chunk\\n".into(),
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
            })
        }
    }

    #[tokio::test]
    async fn output_is_pollable_while_task_is_running() {
        let emitted_first = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mgr = BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(StreamingSandbox {
                emitted_first: Arc::clone(&emitted_first),
                release: Arc::clone(&release),
            }),
            PathBuf::from("/tmp"),
        );
        // Register the waiter before spawn: Notify::notify_waiters does not
        // retain a permit for waiters that have not been enabled yet.
        let first_ready = emitted_first.notified();
        tokio::pin!(first_ready);
        first_ready.as_mut().enable();
        let id = mgr
            .spawn_shell("stream", "echo ignored", Path::new("/tmp"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), &mut first_ready)
            .await
            .expect("sandbox should emit first chunk");

        // The sink callback updates task state asynchronously; yield until
        // polling observes the first chunk while the command is still held.
        let preview = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let output = mgr.output(&id).await.unwrap();
                if output.contains("first chunk") {
                    break output;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("incremental output should become visible before completion");
        assert!(preview.contains("first chunk"));
        assert_eq!(mgr.get(&id).await.unwrap().status, TaskStatus::Running);

        release.notify_waiters();
        let record = mgr.wait(&id).await.unwrap();
        assert_eq!(record.status, TaskStatus::Completed);
        assert_eq!(record.output, "first chunk\\nsecond chunk\\n");
    }

    struct OversizedOutputSandbox;

    #[async_trait::async_trait]
    impl Sandbox for OversizedOutputSandbox {
        fn name(&self) -> &'static str {
            "test-oversized-output"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                ..Default::default()
            }
        }

        async fn exec_shell(&self, _request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            Ok(ShellOutcome {
                // Deliberately ignore ShellRequest::max_output_bytes. The
                // manager must keep persisted task state bounded itself.
                output: "界".repeat(TASK_OUTPUT_MAX_BYTES / "界".len() + 1),
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
            })
        }
    }

    #[tokio::test]
    async fn final_output_from_custom_sandbox_is_capped_at_utf8_boundary() {
        let mgr = BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(OversizedOutputSandbox),
            PathBuf::from("/tmp"),
        );
        let id = mgr
            .spawn_shell("oversized", "ignored", Path::new("/tmp"))
            .await
            .unwrap();
        let record = mgr.wait(&id).await.unwrap();

        assert_eq!(record.status, TaskStatus::Completed);
        assert_eq!(
            record.output.len(),
            TASK_OUTPUT_MAX_BYTES - TASK_OUTPUT_MAX_BYTES % "界".len()
        );
        assert!(record.output.is_char_boundary(record.output.len()));
        assert!(record.output.chars().all(|ch| ch == '界'));
    }

    #[tokio::test]
    async fn failing_command_marked_failed() {
        let mgr = manager();
        let id = mgr
            .spawn_shell("fail", "exit 3", Path::new("/tmp"))
            .await
            .unwrap();
        let rec = mgr.wait(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Failed);
        assert_eq!(rec.exit_code, Some(3));
    }

    #[tokio::test]
    async fn stderr_is_captured() {
        let mgr = manager();
        let id = mgr
            .spawn_shell("err", "echo oops 1>&2; exit 1", Path::new("/tmp"))
            .await
            .unwrap();
        let rec = mgr.wait(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Failed);
        assert!(rec.output.contains("oops"));
    }

    #[tokio::test]
    async fn stop_kills_running_task() {
        let mgr = manager();
        let id = mgr
            .spawn_shell("sleeper", "sleep 30", Path::new("/tmp"))
            .await
            .unwrap();
        // 立即停止（仍在 Running）
        let rec = mgr.stop(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Killed);
        // 再次 stop 幂等（终态原样返回）
        assert_eq!(mgr.stop(&id).await.unwrap().status, TaskStatus::Killed);
    }

    #[tokio::test]
    async fn stop_times_out_when_sandbox_ignores_cancellation() {
        // review 修复回归：自定义沙箱不响应协作式取消时，stop 必须在
        // 宽限期后超时返回（不得无限挂起）；任务保持 Running，可重试。
        let mgr = BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(IgnoreCancelSandbox),
            PathBuf::from("/tmp"),
        )
        .with_stop_grace_period(Duration::from_millis(300));
        let id = mgr
            .spawn_shell("stubborn", "sleep 30", Path::new("/tmp"))
            .await
            .unwrap();
        // 让任务进入 Running 再 stop（stop 对 Running 任务才有取消语义）。
        let stop = mgr.stop(&id).await;
        match stop {
            Err(ToolError::Execution(message)) => {
                assert!(message.contains("grace period"), "{message}");
            }
            other => panic!("expected grace-period timeout, got {other:?}"),
        }
        // 任务未被置 Killed（仍 Running，可稍后重试 stop）。
        let task = mgr.get(&id).await.expect("task record retained");
        assert_eq!(task.status, TaskStatus::Running, "超时不得误标 Killed");
    }

    struct DelayedCancelSandbox(Arc<AtomicBool>);

    #[async_trait::async_trait]
    impl Sandbox for DelayedCancelSandbox {
        fn name(&self) -> &'static str {
            "test-delayed-cancel"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                ..Default::default()
            }
        }

        async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            while !request
                .cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::SeqCst))
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.0.store(true, Ordering::SeqCst);
            Ok(ShellOutcome {
                output: String::new(),
                exit_code: None,
                timed_out: true,
                cancelled: true,
            })
        }
    }

    #[tokio::test]
    async fn concurrent_stops_wait_for_shared_completion() {
        let finished = Arc::new(AtomicBool::new(false));
        let mgr = BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(DelayedCancelSandbox(Arc::clone(&finished))),
            PathBuf::from("/tmp"),
        );
        let id = mgr
            .spawn_shell("delayed", "sleep 1", Path::new("/tmp"))
            .await
            .unwrap();
        let (first, second) = tokio::join!(mgr.stop(&id), mgr.stop(&id));
        assert_eq!(first.unwrap().status, TaskStatus::Killed);
        assert_eq!(second.unwrap().status, TaskStatus::Killed);
        assert!(finished.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn update_progress_and_note() {
        let mgr = manager();
        let id = mgr
            .spawn_shell("t", "sleep 5", Path::new("/tmp"))
            .await
            .unwrap();
        let rec = mgr
            .update(&id, Some(150), Some("halfway".into()))
            .await
            .unwrap();
        assert_eq!(rec.progress, 100); // 钳到 100
        assert_eq!(rec.status_note.as_deref(), Some("halfway"));
        let _ = mgr.stop(&id).await;
    }

    #[tokio::test]
    async fn update_rejects_terminal_tasks() {
        // review 修复回归：终态（Killed/Completed/Failed）任务只读——
        // 任务已落定后 progress/note 更新会造成状态回写与展示不一致。
        let mgr = manager();
        let id = mgr
            .spawn_shell("t", "sleep 5", Path::new("/tmp"))
            .await
            .unwrap();
        mgr.stop(&id).await.unwrap();
        assert!(
            mgr.update(&id, Some(50), Some("late note".into()))
                .await
                .is_err()
        );
        // 终态记录保持原样。
        let rec = mgr.get(&id).await.unwrap();
        assert_eq!(rec.progress, 0);
        assert!(rec.status_note.is_none());
    }

    #[tokio::test]
    async fn missing_task_is_not_found() {
        let mgr = manager();
        assert!(matches!(
            mgr.output("task-nope").await.unwrap_err(),
            ToolError::NotFound(_)
        ));
        assert!(matches!(
            mgr.stop("task-nope").await.unwrap_err(),
            ToolError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn tool_dispatch_run_list_output() {
        let mgr = Arc::new(manager());
        let tool = BackgroundTaskTool::new(Arc::clone(&mgr));
        let mut meta = crate::tools::ToolMetadata::new();
        // 契约要求绝对非根 cwd（与 spawn_shell 校验同口径）。
        let cwd = std::path::Path::new("/tmp");
        let mut ctx = ToolContext {
            cwd,
            metadata: &mut meta,
        };

        // run
        let res = tool
            .execute(
                json!({"action": "run", "command": "echo tool-bg"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!res.is_error);
        let id = res.output.rsplit(' ').next().unwrap().to_string();
        mgr.wait(&id).await.unwrap();

        // output
        let out = tool
            .execute(json!({"action": "output", "task_id": id}), &mut ctx)
            .await
            .unwrap();
        assert!(out.output.contains("tool-bg"));

        // list
        let list = tool
            .execute(json!({"action": "list"}), &mut ctx)
            .await
            .unwrap();
        assert!(list.output.contains("task-"));

        // read-only 判定
        assert!(tool.is_read_only(&json!({"action": "list"})));
        assert!(!tool.is_read_only(&json!({"action": "run"})));

        // 缺 action
        assert!(tool.execute(json!({}), &mut ctx).await.is_err());
    }

    /// 记录 cwd 的沙箱桩：验证 `spawn_shell` 的 cwd 透传到 `ShellRequest`。
    struct CwdRecordingSandbox(Arc<std::sync::Mutex<Option<PathBuf>>>);

    #[async_trait::async_trait]
    impl Sandbox for CwdRecordingSandbox {
        fn name(&self) -> &'static str {
            "test-cwd-recorder"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                ..Default::default()
            }
        }

        async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            *self.0.lock().unwrap() = Some(request.cwd.clone());
            Ok(ShellOutcome {
                output: String::new(),
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
            })
        }
    }

    #[tokio::test]
    async fn spawn_shell_forwards_cwd_to_sandbox() {
        // review 修复：background_task 与 shell_command 同一 cwd 口径
        // （ToolContext.cwd 透传），不再使用进程构造时 cwd（二者可能分叉）。
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let mgr = BackgroundTaskManager::with_sandbox_in_workspace(
            Arc::new(CwdRecordingSandbox(Arc::clone(&recorded))),
            PathBuf::from("/tmp"),
        );
        let id = mgr
            .spawn_shell("cwd-check", "pwd", Path::new("/tmp"))
            .await
            .unwrap();
        let rec = mgr.wait(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Completed);
        assert_eq!(
            *recorded.lock().unwrap(),
            Some(PathBuf::from("/tmp")),
            "spawn_shell must forward the caller-provided cwd to the sandbox"
        );
    }

    #[tokio::test]
    async fn wait_is_idempotent_via_shared_completion_notification() {
        // wait 通过共享完成通知等待，不消费 JoinHandle；多个调用方可安全
        // 并发等待同一个任务，终态记录始终可重复读取。
        let mgr = manager();
        let id = mgr
            .spawn_shell("echo-once", "echo once", Path::new("/tmp"))
            .await
            .unwrap();
        let rec1 = mgr.wait(&id).await.unwrap();
        assert_eq!(rec1.status, TaskStatus::Completed);
        // 二次 wait：共享通知已触发，立即返回同一终态记录。
        let rec2 = mgr.wait(&id).await.unwrap();
        assert_eq!(rec2.id, rec1.id);
        assert_eq!(rec2.status, TaskStatus::Completed);
        // get 同样可用
        assert_eq!(mgr.get(&id).await.unwrap().status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn handle_cleaned_after_background_completion_callback() {
        // 后台完成回调在任务结束后清理 JoinHandle（防内存累积）。
        // 外部通过共享完成通知仍可获取终态记录，但不会再等待 handle。
        let mgr = manager();
        let id = mgr
            .spawn_shell("quick", "echo done", Path::new("/tmp"))
            .await
            .unwrap();
        // 等待自然完成 → 回调已清理 handle
        mgr.wait(&id).await.unwrap();
        // 二次 wait 仍可通过完成通知读取记录
        let rec = mgr.wait(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Completed);
        // stop 终态任务：幂等返回原记录
        let stopped = mgr.stop(&id).await.unwrap();
        assert_eq!(stopped.status, TaskStatus::Completed);
    }
}
