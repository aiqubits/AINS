//! ToolRuntime：统一注册表 + 分发管线（对齐 OpenHarness `tools/base.py::ToolRegistry`
//! + `engine/query.py::_execute_tool_call`）。
//!
//! 分发管线：pre_tool_use hook → 三态权限（允许/询问/拒绝）→ 执行 →
//! 输出 inline/preview 字符预算 → post_tool_use hook。任何环节的拒绝/失败
//! 都归一化为 `is_error` 的 ToolResult 回填，不中止 Agent Loop。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value};

use crate::hooks::{HookEvent, HookExecutor};
use crate::kernel::messages::ToolUse;
use crate::policy::permission_engine::sensitive_path_pattern;
use crate::policy::{
    PermissionDecision, PermissionEngine, PermissionPrompt, PermissionReply, PermissionRequest,
};
use crate::tools::outputs::{ArtifactSink, offload_tool_output_if_needed};
use crate::tools::{Tool, ToolContext, ToolDef, ToolMetadata, ToolResult};

/// 工具注册表 + 分发管线。权限引擎始终存在；`new()` 默认
/// 使用 `default` 模式且无确认回调，因此写操作 fail-closed。hooks 与
/// 外置存储仍可选注入。
pub struct ToolRuntime {
    /// 保持注册序（api_schemas 下发顺序确定性；同名重注册原位替换，
    /// 对齐 Python dict 覆盖语义）。
    tools: Vec<Box<dyn Tool>>,
    index: HashMap<String, usize>,
    permissions: Arc<PermissionEngine>,
    permission_prompt: Option<Arc<dyn PermissionPrompt>>,
    hooks: Option<Arc<HookExecutor>>,
    artifact_sink: Option<Arc<dyn ArtifactSink>>,
    /// 本轮查询的协作式取消标志（Kernel 工具批分发前注入、批后清除）。
    /// 经 [`Tool::set_query_cancel`] 下发给长时工具（shell 等）。
    query_cancel: Mutex<Option<Arc<AtomicBool>>>,
}

impl Default for ToolRuntime {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            permissions: PermissionEngine::new(
                crate::policy::PermissionMode::Default,
                crate::policy::PermissionSettings::default(),
            ),
            permission_prompt: None,
            hooks: None,
            artifact_sink: None,
            query_cancel: Mutex::new(None),
        }
    }
}

impl ToolRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册工具：同名原位替换（保留原注册位置，对齐 dict 覆盖语义）。
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.definition().name;
        match self.index.get(&name) {
            Some(&position) => self.tools[position] = tool,
            None => {
                self.index.insert(name, self.tools.len());
                self.tools.push(tool);
            }
        }
    }

    pub fn with_permissions(
        mut self,
        engine: Arc<PermissionEngine>,
        prompt: Option<Arc<dyn PermissionPrompt>>,
    ) -> Self {
        self.permissions = engine;
        self.permission_prompt = prompt;
        self
    }

    pub fn with_hooks(mut self, hooks: Arc<HookExecutor>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    pub fn with_artifact_sink(mut self, sink: Arc<dyn ArtifactSink>) -> Self {
        self.artifact_sink = Some(sink);
        self
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.index
            .get(name)
            .map(|&position| self.tools[position].as_ref())
    }

    pub fn hooks(&self) -> Option<&Arc<HookExecutor>> {
        self.hooks.as_ref()
    }

    pub fn permissions(&self) -> &Arc<PermissionEngine> {
        &self.permissions
    }

    /// 设置 / 清除本轮查询的协作式取消标志（Kernel 在工具批分发前注入、
    /// 批后传 None 清除）。每次 [`Self::dispatch`] 前都会把当前值（含 None）
    /// 重新注入给目标工具，故陈旧标志不会跨批残留。
    pub fn set_query_cancel(&self, flag: Option<Arc<AtomicBool>>) {
        *self
            .query_cancel
            .lock()
            .expect("query cancel lock poisoned") = flag;
    }

    fn current_query_cancel(&self) -> Option<Arc<AtomicBool>> {
        self.query_cancel
            .lock()
            .expect("query cancel lock poisoned")
            .clone()
    }

    /// A cancellation request belongs to the Kernel, which performs the
    /// check-and-clear and emits the terminal status.  The runtime must only
    /// observe it: clearing it here would allow a later tool in the same batch
    /// to run after the user pressed Stop.
    fn query_is_cancelled(&self) -> bool {
        self.current_query_cancel()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    fn cancelled_result() -> ToolResult {
        ToolResult::err("Tool execution interrupted by user")
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 全部工具的 API schema（对齐 `ToolRegistry.to_api_schema`）。
    pub fn api_schemas(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    /// 单个 tool_use 的完整分发管线。未知工具与执行异常均归一化为
    /// is_error 的 ToolResult（对齐基线合成 error tool_result 语义）。
    pub async fn dispatch(&self, tool_use: &ToolUse, ctx: &mut ToolContext<'_>) -> ToolResult {
        // Stop may arrive while another tool in this batch is awaiting a
        // permission reply.  Never start a later hook/prompt/tool after it;
        // the Kernel remains responsible for consuming the flag at the batch
        // boundary and reporting QUERY_INTERRUPTED_STATUS.
        if self.query_is_cancelled() {
            return self.apply_output_budget(tool_use, Self::cancelled_result(), ctx);
        }
        // 1. pre_tool_use hook：任一 blocked 即拒绝执行
        if let Some(hooks) = &self.hooks {
            let mut payload = Map::new();
            payload.insert("tool_name".into(), Value::String(tool_use.name.clone()));
            payload.insert("tool_input".into(), tool_use.input.clone());
            payload.insert(
                "event".into(),
                Value::String(HookEvent::PreToolUse.as_str().into()),
            );
            let pre = hooks.execute(HookEvent::PreToolUse, &payload).await;
            if pre.blocked() {
                let reason = pre.reason();
                let result = ToolResult::err(if reason.is_empty() {
                    format!("pre_tool_use hook blocked {}", tool_use.name)
                } else {
                    reason
                });
                return self.apply_output_budget(tool_use, result, ctx);
            }
        }

        // 2. 工具解析
        let Some(tool) = self.get(&tool_use.name) else {
            let result = ToolResult::err(format!("Unknown tool: {}", tool_use.name));
            return self.apply_output_budget(tool_use, result, ctx);
        };

        // 3. 三态权限：路径/命令归一化后求值（对齐 _resolve_permission_file_path）
        {
            let engine = &self.permissions;
            let file_path = resolve_permission_file_path(&tool_use.name, ctx.cwd, &tool_use.input);
            let command = extract_permission_command(&tool_use.input);
            let permission_tool_name = if tool_use.name == "background_task"
                && matches!(
                    tool_use.input.get("action").and_then(Value::as_str),
                    Some("run" | "stop")
                ) {
                // `background_task/run` is an arbitrary shell execution surface;
                // it must share shell_command's deny/allow identity. `stop`
                // terminates the process spawned by `run`, so it belongs to the
                // same authorization surface — otherwise a user who approved
                // `run` (persisted under shell_command) cannot stop the task
                // without a fresh prompt ("can start but cannot stop").
                "shell_command"
            } else {
                &tool_use.name
            };
            let decision = if let Some(path) = file_path.as_deref()
                // 内置敏感路径防护优先：无论工具与模式一律拒绝（对齐 evaluate
                // 内部恒检查语义）。必须置于复合工具读象限分支之前——
                // file_access_allowed 对敏感路径同样返回 false，若后者先判，
                // todo_write/edit_file 的敏感路径拒绝会变成 "not readable..."
                // 文案，与 web 契约测试断言的 "sensitive credential path" 不一致
                // （review 修复：历史该分支先于 evaluate 拦截敏感路径）。
                && let Some(pattern) = sensitive_path_pattern(path)
            {
                PermissionDecision::deny(format!(
                    "Access denied: {path} is a sensitive credential path \
                     (matched built-in pattern '{pattern}')"
                ))
            } else if let Some(path) = file_path.as_deref()
                // edit_file 与 todo_write 都是先读旧内容再写的复合操作：
                // 写象限之外还必须过读象限（deny_read 区域的 read-modify-write
                // 实际发生读取；review 修复：历史 todo_write 只走写象限，
                // 与 edit_file 保护不对称）。
                && matches!(tool_use.name.as_str(), "edit_file" | "todo_write")
                && !engine.file_access_allowed(path, true)
            {
                PermissionDecision::deny(format!(
                    "Access denied: {path} is not readable under the sandbox filesystem policy"
                ))
            } else if let Some(path) = file_path.as_deref()
                && matches!(tool_use.name.as_str(), "glob" | "grep")
                && !engine.recursive_read_allowed(path)
            {
                PermissionDecision::deny(format!(
                    "Recursive access denied: {path} cannot be authorized with the configured sandbox deny rules"
                ))
            } else {
                engine.evaluate(
                    permission_tool_name,
                    tool.is_read_only(&tool_use.input),
                    file_path.as_deref(),
                    command.as_deref(),
                )
            };
            if !decision.allowed {
                if decision.requires_confirmation
                    && let Some(prompt) = &self.permission_prompt
                {
                    // 询问前发 notification hook（对齐基线 permission_prompt 通知）
                    if let Some(hooks) = &self.hooks {
                        let mut payload = Map::new();
                        payload.insert(
                            "event".into(),
                            Value::String(HookEvent::Notification.as_str().into()),
                        );
                        payload.insert(
                            "notification_type".into(),
                            Value::String("permission_prompt".into()),
                        );
                        payload.insert("tool_name".into(), Value::String(tool_use.name.clone()));
                        payload.insert("reason".into(), Value::String(decision.reason.clone()));
                        payload.insert("tool_input".into(), tool_use.input.clone());
                        if let Some(path) = &file_path {
                            payload
                                .insert("resolved_file_path".into(), Value::String(path.clone()));
                        }
                        if let Some(command) = &command {
                            payload.insert("command".into(), Value::String(command.clone()));
                        }
                        let _ = hooks.execute(HookEvent::Notification, &payload).await;
                    }
                    let request = PermissionRequest {
                        tool_name: tool_use.name.clone(),
                        reason: decision.reason.clone(),
                        tool_input: tool_use.input.clone(),
                        resolved_file_path: file_path.clone(),
                        command: command.clone(),
                    };
                    let mut always_allow_persist = false;
                    match prompt.confirm(&request, self.current_query_cancel()).await {
                        PermissionReply::Allow => {}
                        PermissionReply::AlwaysAllow => {
                            // `background_task/run` shares shell_command's
                            // permission identity.  Persist the identity
                            // actually evaluated; storing the display tool
                            // name made "Always allow" ineffective for every
                            // later background run.
                            always_allow_persist = true;
                        }
                        PermissionReply::Deny => {
                            let result = ToolResult::err(permission_denied_message(
                                &decision.reason,
                                &tool_use.name,
                            ));
                            return self.apply_output_budget(tool_use, result, ctx);
                        }
                    }
                    // A prompt can resolve with Allow after the user pressed
                    // Stop (for example a click already queued by the UI).
                    // Do not let that stale approval authorize a mutation.
                    if self.query_is_cancelled() {
                        return self.apply_output_budget(tool_use, Self::cancelled_result(), ctx);
                    }
                    // Stop 竞态下“总是允许”不得静默持久化：取消检查通过后
                    // 才写会话放行集，避免用户以为未生效、实际已放行
                    // （review 修复：历史在取消检查前持久化）。
                    if always_allow_persist {
                        engine.allow_for_session(permission_tool_name);
                    }
                } else {
                    let result = ToolResult::err(permission_denied_message(
                        &decision.reason,
                        &tool_use.name,
                    ));
                    return self.apply_output_budget(tool_use, result, ctx);
                }
            }
        }

        // 4. 执行（Err 归一化为 is_error tool_result）。先注入查询级取消标志
        //    （含 None 清除旧值），长时工具据此响应 UI 中断。
        if self.query_is_cancelled() {
            return self.apply_output_budget(tool_use, Self::cancelled_result(), ctx);
        }
        tool.set_query_cancel(self.current_query_cancel());
        let result = match tool.execute(tool_use.input.clone(), ctx).await {
            Ok(result) => result,
            Err(error) => ToolResult::err(format!("Tool {} failed: {error}", tool_use.name)),
        };

        // 5. 输出预算：超长外置 + 内联预览
        let result = self.apply_output_budget(tool_use, result, ctx);

        // 6. post_tool_use hook：观察性执行，结果不改写 tool_result（对齐基线）
        if let Some(hooks) = &self.hooks {
            let mut payload = Map::new();
            payload.insert("tool_name".into(), Value::String(tool_use.name.clone()));
            payload.insert("tool_input".into(), tool_use.input.clone());
            payload.insert("tool_output".into(), Value::String(result.output.clone()));
            payload.insert("tool_is_error".into(), Value::Bool(result.is_error));
            payload.insert(
                "event".into(),
                Value::String(HookEvent::PostToolUse.as_str().into()),
            );
            let _ = hooks.execute(HookEvent::PostToolUse, &payload).await;
        }
        result
    }

    /// 并发分发同一 assistant turn 中的多个工具调用。每个工具获得
    /// 跨轮状态袋的独立快照，完成后按 tool_use 原始顺序合并增量；
    /// 因此慢工具不会阻塞其它工具，而回填顺序仍稳定。
    pub async fn dispatch_many(
        &self,
        tool_uses: &[ToolUse],
        cwd: &Path,
        metadata: &mut ToolMetadata,
    ) -> Vec<ToolResult> {
        if tool_uses.len() == 1 {
            let mut ctx = ToolContext { cwd, metadata };
            return vec![self.dispatch(&tool_uses[0], &mut ctx).await];
        }
        if tool_uses.is_empty() {
            return Vec::new();
        }

        // 同一批次若存在相同的独占资源键，必须共享最新 metadata 顺序执行。
        // 典型场景是 Web todo_write：每次调用都会读改写同一个完整 Markdown
        // 文档，快照并发 + last-writer-wins 会让先完成的更新静默丢失。
        let mut exclusive_keys = std::collections::HashSet::new();
        let has_conflict = tool_uses.iter().any(|tool_use| {
            self.get(&tool_use.name)
                .and_then(|tool| tool.exclusive_execution_key(&tool_use.input, cwd))
                .is_some_and(|key| !exclusive_keys.insert(key))
        });
        if has_conflict {
            let mut results = Vec::with_capacity(tool_uses.len());
            for tool_use in tool_uses {
                let mut ctx = ToolContext { cwd, metadata };
                results.push(self.dispatch(tool_use, &mut ctx).await);
            }
            return results;
        }

        let baseline = metadata.clone();
        let executions = tool_uses.iter().map(|tool_use| {
            let mut local_metadata = baseline.clone();
            async move {
                let mut ctx = ToolContext {
                    cwd,
                    metadata: &mut local_metadata,
                };
                let result = self.dispatch(tool_use, &mut ctx).await;
                (result, local_metadata)
            }
        });
        let completed = futures::future::join_all(executions).await;
        let mut results = Vec::with_capacity(completed.len());
        for (result, local_metadata) in completed {
            merge_metadata_delta(&baseline, &local_metadata, metadata);
            results.push(result);
        }
        results
    }

    fn apply_output_budget(
        &self,
        tool_use: &ToolUse,
        result: ToolResult,
        ctx: &mut ToolContext<'_>,
    ) -> ToolResult {
        let offloaded = offload_tool_output_if_needed(
            &tool_use.name,
            &tool_use.id,
            result.output,
            self.artifact_sink.as_deref(),
        );
        if let Some(artifact) = &offloaded.artifact {
            // 外置引用记入独立的活跃工件列表（对齐 _remember_active_artifact，
            // 不占 work_log 配额）
            ctx.metadata.record_active_artifact(artifact.clone());
        }
        ToolResult {
            output: offloaded.inline,
            is_error: result.is_error,
            metadata: result.metadata,
        }
    }
}

/// 把单个并发工具相对于起始快照的变化合并回会话状态袋。
/// 列表用“起始快照子序列前缀”识别 capped-unique 操作中新增/
/// 移到末尾的条目；
/// `extra` 的同键冲突按 tool_use 顺序 last-writer-wins。
fn merge_metadata_delta(baseline: &ToolMetadata, local: &ToolMetadata, target: &mut ToolMetadata) {
    replay_list_delta(&baseline.read_files, &local.read_files, |value| {
        target.record_read_file(value)
    });
    replay_list_delta(&baseline.invoked_skills, &local.invoked_skills, |value| {
        target.record_invoked_skill(value)
    });
    replay_list_delta(&baseline.work_log, &local.work_log, |value| {
        target.append_work_log(value)
    });
    replay_list_delta(
        &baseline.active_artifacts,
        &local.active_artifacts,
        |value| target.record_active_artifact(value),
    );
    if local.user_goal != baseline.user_goal {
        target.user_goal.clone_from(&local.user_goal);
    }
    for key in baseline.extra.keys() {
        if !local.extra.contains_key(key) {
            target.extra.remove(key);
        }
    }
    for (key, value) in &local.extra {
        if baseline.extra.get(key) != Some(value) {
            target.extra.insert(key.clone(), value.clone());
        }
    }
}

fn replay_list_delta<F>(baseline: &[String], local: &[String], mut replay: F)
where
    F: FnMut(String),
{
    let mut baseline_cursor = 0usize;
    let mut unchanged_prefix = 0usize;
    for value in local {
        let Some(offset) = baseline[baseline_cursor..]
            .iter()
            .position(|baseline_value| baseline_value == value)
        else {
            break;
        };
        baseline_cursor += offset + 1;
        unchanged_prefix += 1;
        if baseline_cursor == baseline.len() {
            break;
        }
    }
    for value in &local[unchanged_prefix..] {
        replay(value.clone());
    }
}

fn permission_denied_message(reason: &str, tool_name: &str) -> String {
    if reason.is_empty() {
        format!("Permission denied for {tool_name}")
    } else {
        reason.to_string()
    }
}

/// 权限求值用路径归一化（对齐 `_resolve_permission_file_path`）：
/// `file_path` / `path` / `root` 首个非空字符串字段，相对路径以 cwd 为锚，
/// 展开 `~`、词法解析为绝对路径，并在 Native 端解析目标或最近存在父目录
/// 的 symlink。glob 的绝对 pattern 按其真实执行优先级覆盖 root。
fn resolve_permission_file_path(tool_name: &str, cwd: &Path, input: &Value) -> Option<String> {
    let object = input.as_object()?;
    // shell always has an effective working directory. Include it in policy
    // evaluation even when the caller omitted the optional override.
    if tool_name == "shell_command" {
        let candidate = object.get("cwd").and_then(Value::as_str).unwrap_or(".");
        return Some(resolve_permission_candidate(cwd, candidate));
    }
    if tool_name == "background_task" && object.get("action").and_then(Value::as_str) == Some("run")
    {
        return Some(resolve_permission_candidate(cwd, "."));
    }
    // glob ignores `root` when `pattern` is absolute, so the permission path
    // must follow the same precedence or an absolute sensitive pattern could
    // be checked against an unrelated safe root.
    if tool_name == "glob"
        && let Some(pattern) = object.get("pattern").and_then(Value::as_str)
        && Path::new(pattern).is_absolute()
    {
        return Some(resolve_permission_candidate(cwd, pattern));
    }
    for key in ["file_path", "path", "root"] {
        if let Some(value) = object.get(key).and_then(Value::as_str)
            && !value.trim().is_empty()
        {
            return Some(resolve_permission_candidate(cwd, value));
        }
    }
    None
}

fn resolve_permission_candidate(cwd: &Path, candidate: &str) -> String {
    let path = expand_tilde(candidate, home_dir());
    let anchored = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let resolved = lexical_resolve_path(&anchored);
    #[cfg(not(target_arch = "wasm32"))]
    let resolved = resolve_existing_ancestor(&resolved);
    resolved.display().to_string()
}

/// `~` 展开用 home 目录：`HOME` 优先，Windows 常见的 `USERPROFILE`
/// 回退（Native 桌面目标含 Windows，`HOME` 常未设置；权限求值与
/// 实际文件访问必须同口径展开，否则 `$HOME` 锚定规则会静默失配）。
pub(crate) fn home_dir() -> Option<PathBuf> {
    select_home(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

/// 纯函数口径，便于并行测试不碰 `set_var`。
fn select_home(
    home: Option<std::ffi::OsString>,
    userprofile: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    home.or(userprofile).map(PathBuf::from)
}

fn expand_tilde(candidate: &str, home: Option<PathBuf>) -> PathBuf {
    if (candidate == "~" || candidate.starts_with("~/"))
        && let Some(home) = home
    {
        return home.join(candidate.trim_start_matches("~/"));
    }
    PathBuf::from(candidate)
}

/// 词法 resolve（`.`/`..` 消解，不追符号链接；与 policy::sandbox 同口径）。
fn lexical_resolve_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    normalized.components().next_back(),
                    None | Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolve symlinks in the target itself or its nearest existing ancestor.
/// This keeps permission checks aligned with the path the OS will access even
/// when a write creates a new file below a symlinked directory.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let mut cursor = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(mut canonical) = std::fs::canonicalize(cursor) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let Some(name) = cursor.file_name() else {
            return path.to_path_buf();
        };
        missing.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        cursor = parent;
    }
}

/// 权限求值用命令提取（对齐 `_extract_permission_command`）。
fn extract_permission_command(input: &Value) -> Option<String> {
    let value = input.as_object()?.get("command")?.as_str()?;
    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::error::ToolError;
    use crate::policy::{PermissionMode, PermissionSettings};
    use crate::tools::{ToolCategory, ToolMetadata};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct EchoTool {
        name: &'static str,
        read_only: bool,
    }

    struct ExclusiveCounterTool;

    struct AlwaysAllowPrompt(AtomicUsize);

    struct CancelThenAllowPrompt(Arc<AtomicBool>);

    struct CancelThenAlwaysAllowPrompt(Arc<AtomicBool>);

    struct CountingMutatingTool(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl PermissionPrompt for AlwaysAllowPrompt {
        async fn confirm(
            &self,
            _request: &PermissionRequest,
            _cancel: Option<Arc<AtomicBool>>,
        ) -> PermissionReply {
            self.0.fetch_add(1, Ordering::SeqCst);
            PermissionReply::AlwaysAllow
        }
    }

    #[async_trait::async_trait]
    impl PermissionPrompt for CancelThenAllowPrompt {
        async fn confirm(
            &self,
            _request: &PermissionRequest,
            _cancel: Option<Arc<AtomicBool>>,
        ) -> PermissionReply {
            self.0.store(true, Ordering::Release);
            PermissionReply::Allow
        }
    }

    #[async_trait::async_trait]
    impl PermissionPrompt for CancelThenAlwaysAllowPrompt {
        async fn confirm(
            &self,
            _request: &PermissionRequest,
            _cancel: Option<Arc<AtomicBool>>,
        ) -> PermissionReply {
            self.0.store(true, Ordering::Release);
            PermissionReply::AlwaysAllow
        }
    }

    #[async_trait::async_trait]
    impl Tool for CountingMutatingTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "counting_mutator".into(),
                description: "records a mutation".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(
            &self,
            _input: Value,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::ok("mutated"))
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::AgentInternal
        }
    }

    #[async_trait::async_trait]
    impl Tool for ExclusiveCounterTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "exclusive_counter".into(),
                description: "increment a metadata counter".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        fn is_read_only(&self, _input: &Value) -> bool {
            true
        }

        fn exclusive_execution_key(&self, _input: &Value, _cwd: &Path) -> Option<String> {
            Some("counter".into())
        }

        async fn execute(
            &self,
            _input: Value,
            ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            let next = ctx
                .metadata
                .extra
                .get("counter")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            ctx.metadata
                .extra
                .insert("counter".into(), serde_json::json!(next));
            Ok(ToolResult::ok(next.to_string()))
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::AgentInternal
        }
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: self.name.to_string(),
                description: "echo input back".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                }),
            }
        }

        fn is_read_only(&self, _input: &Value) -> bool {
            self.read_only
        }

        async fn execute(
            &self,
            input: Value,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok(
                input
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ))
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Compute
        }
    }

    fn tool_use(name: &str, input: Value) -> ToolUse {
        ToolUse {
            id: "tu_1".into(),
            name: name.into(),
            input,
        }
    }

    fn runtime_with_echo() -> ToolRuntime {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "echo",
            read_only: true,
        }));
        runtime
    }

    #[tokio::test]
    async fn registry_lookup_and_schema_ordering() {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "beta",
            read_only: true,
        }));
        runtime.register(Box::new(EchoTool {
            name: "alpha",
            read_only: true,
        }));
        assert_eq!(runtime.len(), 2);
        assert!(runtime.get("beta").is_some());
        assert!(runtime.get("missing").is_none());
        // schema 顺序 = 注册序（非字母序）
        let names: Vec<String> = runtime
            .api_schemas()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert_eq!(names, vec!["beta", "alpha"]);
    }

    #[tokio::test]
    async fn reregistering_same_name_replaces_in_place() {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "echo",
            read_only: false,
        }));
        runtime.register(Box::new(EchoTool {
            name: "other",
            read_only: true,
        }));
        runtime.register(Box::new(EchoTool {
            name: "echo",
            read_only: true,
        }));
        assert_eq!(runtime.len(), 2);
        // 替换后 is_read_only 生效且位置保持
        assert!(runtime.get("echo").unwrap().is_read_only(&Value::Null));
        let names: Vec<String> = runtime
            .api_schemas()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert_eq!(names, vec!["echo", "other"]);
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_is_error_result() {
        let runtime = ToolRuntime::new();
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(&tool_use("nope", Value::Null), &mut ctx)
            .await;
        assert!(result.is_error);
        assert_eq!(result.output, "Unknown tool: nope");
    }

    #[tokio::test]
    async fn dispatch_executes_and_applies_budget() {
        let runtime = runtime_with_echo();
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let long_text = "a".repeat(crate::tools::outputs::DEFAULT_TOOL_OUTPUT_INLINE_CHARS + 10);
        let result = runtime
            .dispatch(
                &tool_use("echo", serde_json::json!({"text": long_text})),
                &mut ctx,
            )
            .await;
        assert!(!result.is_error);
        assert!(result.output.starts_with("[Tool output truncated]"));
    }

    #[tokio::test]
    async fn default_runtime_fails_closed_for_mutating_tools() {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "mutating_echo",
            read_only: false,
        }));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(
                &tool_use("mutating_echo", serde_json::json!({"text": "must not run"})),
                &mut ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("require user confirmation"));
    }

    #[tokio::test]
    async fn background_task_always_allow_is_stored_under_shell_permission_identity() {
        // background_task/run 以 shell_command 身份求值；“总是允许”也必须
        // 写入同一身份，否则每次后台任务都会再次弹窗。
        let prompt = Arc::new(AlwaysAllowPrompt(AtomicUsize::new(0)));
        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::Default,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(engine, Some(prompt.clone()));
        runtime.register(Box::new(EchoTool {
            name: "background_task",
            read_only: false,
        }));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let run = || {
            tool_use(
                "background_task",
                serde_json::json!({"action": "run", "command": "echo task"}),
            )
        };

        assert!(!runtime.dispatch(&run(), &mut ctx).await.is_error);
        assert!(!runtime.dispatch(&run(), &mut ctx).await.is_error);
        assert_eq!(prompt.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn background_task_stop_shares_shell_permission_identity() {
        // review 修复回归：`background_task/stop` 终止 `run` 启动的进程，必须
        // 与 `run` 同属 shell_command 授权面——否则用户批准 run（持久化在
        // shell_command 身份下）后 stop 仍需弹窗（"能起不能停"）。
        let prompt = Arc::new(AlwaysAllowPrompt(AtomicUsize::new(0)));
        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::Default,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(engine, Some(prompt.clone()));
        runtime.register(Box::new(EchoTool {
            name: "background_task",
            read_only: false,
        }));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let run = || {
            tool_use(
                "background_task",
                serde_json::json!({"action": "run", "command": "echo task"}),
            )
        };
        let stop = || {
            tool_use(
                "background_task",
                serde_json::json!({"action": "stop", "task_id": "task-1"}),
            )
        };

        // run 首次弹窗并把 AlwaysAllow 持久化到 shell_command 身份。
        assert!(!runtime.dispatch(&run(), &mut ctx).await.is_error);
        assert_eq!(prompt.0.load(Ordering::SeqCst), 1);
        // stop 复用 shell_command 放行，不再弹窗（修复前按 background_task
        // 身份求值 → 未授权 → 弹第 2 次）。
        assert!(!runtime.dispatch(&stop(), &mut ctx).await.is_error);
        assert_eq!(prompt.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_after_permission_allow_prevents_mutation() {
        // Regression: Stop can race a visible permission dialog.  Even if an
        // Allow click was already queued, it must not authorize execution.
        let cancel = Arc::new(AtomicBool::new(false));
        let executed = Arc::new(AtomicUsize::new(0));
        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::Default,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(
            engine,
            Some(Arc::new(CancelThenAllowPrompt(Arc::clone(&cancel)))),
        );
        runtime.set_query_cancel(Some(Arc::clone(&cancel)));
        runtime.register(Box::new(CountingMutatingTool(Arc::clone(&executed))));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };

        let result = runtime
            .dispatch(&tool_use("counting_mutator", Value::Null), &mut ctx)
            .await;

        assert!(result.is_error);
        assert!(result.output.contains("interrupted by user"));
        assert_eq!(executed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancellation_after_always_allow_does_not_persist_session_allow() {
        // review 修复回归：Stop 竞态下用户点"总是允许"时，工具不执行
        // （取消检查拦截），但会话放行集不得被静默持久化——否则下一次
        // 查询该工具直接放行，用户以为"没生效"实际已授权。
        let cancel = Arc::new(AtomicBool::new(false));
        let executed = Arc::new(AtomicUsize::new(0));
        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::Default,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(
            Arc::clone(&engine),
            Some(Arc::new(CancelThenAlwaysAllowPrompt(Arc::clone(&cancel)))),
        );
        runtime.set_query_cancel(Some(Arc::clone(&cancel)));
        runtime.register(Box::new(CountingMutatingTool(Arc::clone(&executed))));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };

        let result = runtime
            .dispatch(&tool_use("counting_mutator", Value::Null), &mut ctx)
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("interrupted by user"));
        assert_eq!(executed.load(Ordering::SeqCst), 0);
        // 会话放行集未被污染：取消清除后再次 dispatch 仍需走确认（弹窗
        // 再次出现）并真正执行——证明第一次的 AlwaysAllow 未持久化。
        runtime.set_query_cancel(None);
        let second = runtime
            .dispatch(&tool_use("counting_mutator", Value::Null), &mut ctx)
            .await;
        assert!(
            !second.is_error,
            "second dispatch must re-confirm and execute: {second:?}"
        );
        assert_eq!(executed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn todo_write_respects_deny_read_quadrant() {
        // review 修复回归：todo_write 是 read-modify-write 复合操作（Native
        // 端读全文再写），deny_read 区域的 TODO 文件不得被读取——历史只走
        // 写象限，与 edit_file 的读象限保护不对称（侧信道 + 配置预期违背）。
        use crate::policy::sandbox_policy::FilesystemPolicy;
        use crate::tools::interact::TodoWriteTool;
        let engine = crate::policy::PermissionEngine::with_filesystem_policy(
            PermissionMode::Default,
            PermissionSettings::default(),
            FilesystemPolicy {
                deny_read: vec!["/tmp/secret/*".into()],
                ..Default::default()
            },
        );
        let mut runtime = ToolRuntime::new().with_permissions(engine, None);
        runtime.register(Box::new(TodoWriteTool));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp/secret"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(
                &tool_use(
                    "todo_write",
                    serde_json::json!({"item": "x", "path": "TODO.md"}),
                ),
                &mut ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(
            result
                .output
                .contains("not readable under the sandbox filesystem policy"),
            "{result:?}"
        );
        // 控制组：无 deny_read 时 todo_write 正常执行（读象限放行；
        // FullAuto 免确认）。
        let open_engine = crate::policy::PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings::default(),
        );
        let mut open_runtime = ToolRuntime::new().with_permissions(open_engine, None);
        open_runtime.register(Box::new(TodoWriteTool));
        let temp_cwd = std::env::temp_dir();
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: &temp_cwd,
            metadata: &mut metadata,
        };
        let ok = open_runtime
            .dispatch(
                &tool_use(
                    "todo_write",
                    serde_json::json!({"item": "x", "path": "ains-todo-test.md"}),
                ),
                &mut ctx,
            )
            .await;
        assert!(!ok.is_error, "unrestricted todo_write must succeed: {ok:?}");
    }

    #[tokio::test]
    async fn conflicting_exclusive_keys_execute_against_latest_metadata() {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(ExclusiveCounterTool));
        let mut metadata = ToolMetadata::new();
        let results = runtime
            .dispatch_many(
                &[
                    tool_use("exclusive_counter", Value::Null),
                    tool_use("exclusive_counter", Value::Null),
                ],
                Path::new("/tmp"),
                &mut metadata,
            )
            .await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].output, "1");
        assert_eq!(results[1].output, "2");
        assert_eq!(metadata.extra["counter"], serde_json::json!(2));
    }

    #[test]
    fn concurrent_metadata_merge_replays_capped_unique_operations() {
        let mut baseline = ToolMetadata::new();
        baseline.append_work_log("a");
        baseline.append_work_log("b");
        baseline.append_work_log("c");
        baseline.extra.insert("stable".into(), serde_json::json!(1));

        let mut first = baseline.clone();
        first.append_work_log("b");
        first.append_work_log("x");
        first.extra.insert("first".into(), serde_json::json!(true));
        let mut second = baseline.clone();
        second.append_work_log("a");
        second
            .extra
            .insert("second".into(), serde_json::json!(true));

        let mut merged = baseline.clone();
        merge_metadata_delta(&baseline, &first, &mut merged);
        merge_metadata_delta(&baseline, &second, &mut merged);
        assert_eq!(merged.work_log, vec!["c", "b", "x", "a"]);
        assert_eq!(merged.extra["stable"], serde_json::json!(1));
        assert_eq!(merged.extra["first"], serde_json::json!(true));
        assert_eq!(merged.extra["second"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn permission_file_path_extraction_and_normalization() {
        let cwd = Path::new("/work/project");
        let input = serde_json::json!({"path": "src/../secrets/key.pem"});
        assert_eq!(
            resolve_permission_file_path("echo", cwd, &input),
            Some(
                std::fs::canonicalize("/work")
                    .unwrap_or_else(|_| PathBuf::from("/work"))
                    .join("project/secrets/key.pem")
                    .display()
                    .to_string()
            )
        );
        // file_path 优先于 path / root
        let input = serde_json::json!({"root": "/r", "file_path": "/abs/x", "path": "rel"});
        assert_eq!(
            resolve_permission_file_path("echo", cwd, &input),
            Some("/abs/x".to_string())
        );
        // 空白值跳过
        let input = serde_json::json!({"path": "  ", "root": "/only/root"});
        assert_eq!(
            resolve_permission_file_path("echo", cwd, &input),
            Some("/only/root".to_string())
        );
        assert_eq!(
            resolve_permission_file_path("echo", cwd, &Value::Null),
            None
        );
        assert_eq!(
            expand_tilde("~/.ssh/id_rsa", Some(PathBuf::from("/home/alice"))),
            PathBuf::from("/home/alice/.ssh/id_rsa")
        );
        // home 选择：HOME 优先，USERPROFILE 回退（Windows 常见），
        // 纯函数口径避免并行测试碰 set_var。
        assert_eq!(
            select_home(Some("/home/a".into()), Some("C:/Users/a".into())),
            Some(PathBuf::from("/home/a"))
        );
        assert_eq!(
            select_home(None, Some("C:/Users/a".into())),
            Some(PathBuf::from("C:/Users/a"))
        );
        assert_eq!(select_home(None, None), None);
        assert_eq!(
            resolve_permission_file_path(
                "glob",
                cwd,
                &serde_json::json!({"root": "/safe", "pattern": "/home/u/.ssh/*"})
            ),
            Some("/home/u/.ssh/*".to_string())
        );
        assert_eq!(
            resolve_permission_file_path(
                "shell_command",
                cwd,
                &serde_json::json!({"command": "pwd", "cwd": "subdir"})
            ),
            Some("/work/project/subdir".to_string())
        );
        assert_eq!(
            resolve_permission_file_path(
                "shell_command",
                cwd,
                &serde_json::json!({"command": "pwd"})
            ),
            Some("/work/project".to_string())
        );
        assert_eq!(
            resolve_permission_file_path(
                "background_task",
                cwd,
                &serde_json::json!({"action": "run", "command": "pwd"})
            ),
            Some("/work/project".to_string())
        );
        assert_eq!(
            extract_permission_command(&serde_json::json!({"command": "ls -la"})),
            Some("ls -la".to_string())
        );
        assert_eq!(
            extract_permission_command(&serde_json::json!({"command": "  "})),
            None
        );
    }

    #[tokio::test]
    async fn dispatch_denies_sensitive_path_via_engine() {
        use crate::policy::{PermissionMode, PermissionSettings};
        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(engine, None);
        runtime.register(Box::new(EchoTool {
            name: "echo",
            read_only: true,
        }));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/home/user"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(
                &tool_use(
                    "echo",
                    serde_json::json!({"path": ".ssh/id_rsa", "text": "x"}),
                ),
                &mut ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("sensitive credential path"));
    }

    #[tokio::test]
    async fn dispatch_denies_todo_write_sensitive_path_with_canonical_message() {
        // 回归：复合工具（todo_write）访问敏感路径必须返回与 evaluate 一致的
        // "sensitive credential path" 文案。历史实现先判读象限分支，敏感路径被
        // file_access_allowed 拦截但文案变成 "not readable..."，与 wasm 契约
        // 测试（web_tools.rs）断言不一致（web 端 CI 红）。
        use crate::policy::{PermissionMode, PermissionSettings};
        use crate::tools::interact::TodoWriteTool;
        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(engine, None);
        runtime.register(Box::new(TodoWriteTool));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(
                &tool_use(
                    "todo_write",
                    serde_json::json!({"item": "x", "path": "/home/u/.ssh/notes.md"}),
                ),
                &mut ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("sensitive credential path"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn dispatch_denies_edit_file_sensitive_path_with_canonical_message() {
        // edit_file 与 todo_write 共享复合工具读象限分支，且真实攻击面更常见
        // （read-modify-write 会先读取旧内容），锁定其敏感路径拒绝文案防回归。
        use crate::policy::{PermissionMode, PermissionSettings};
        use crate::tools::filesystem::FileEditTool;
        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(engine, None);
        runtime.register(Box::new(FileEditTool));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(
                &tool_use(
                    "edit_file",
                    serde_json::json!({
                        "path": "/home/u/.ssh/notes.md",
                        "old_str": "a",
                        "new_str": "b",
                    }),
                ),
                &mut ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("sensitive credential path"));
        assert!(!result.output.contains("not readable"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn dispatch_denies_glob_sensitive_root_with_canonical_message() {
        // glob/grep 的敏感 root 拒绝文案与 evaluate 统一（review 修复后走
        // "sensitive credential path" 而非 "Recursive access denied"）。
        // 目录根经 policy_match_paths 尾随斜杠变体命中 */.ssh/* 规则。
        use crate::policy::{PermissionMode, PermissionSettings};
        use crate::tools::filesystem::GlobTool;
        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(engine, None);
        runtime.register(Box::new(GlobTool));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(
                &tool_use(
                    "glob",
                    serde_json::json!({"root": "/home/u/.ssh", "pattern": "*.pem"}),
                ),
                &mut ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("sensitive credential path"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_denies_sensitive_path_through_symlink() {
        use crate::policy::{PermissionMode, PermissionSettings};
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa"), "secret").unwrap();
        symlink(&ssh, root.path().join("linked")).unwrap();

        let engine = crate::policy::PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings::default(),
        );
        let mut runtime = ToolRuntime::new().with_permissions(engine, None);
        runtime.register(Box::new(crate::tools::filesystem::FileReadTool));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: root.path(),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(
                &tool_use("read_file", serde_json::json!({"path": "linked/id_rsa"})),
                &mut ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("sensitive credential path"));
        assert!(!result.output.contains("secret"));
    }
}
