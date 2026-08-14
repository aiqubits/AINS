//! ToolRuntime：统一注册表 + 分发管线（对齐 Harness `tools/base.py::ToolRegistry`
//! + `engine/query.py::_execute_tool_call`）。
//!
//! 分发管线：pre_tool_use hook → 三态权限（允许/询问/拒绝）→ 执行 →
//! 输出 inline/preview 字符预算 → post_tool_use hook。任何环节的拒绝/失败
//! 都归一化为 `is_error` 的 ToolResult 回填，不中止 Agent Loop。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

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
///
/// 工具活跃状态：`disabled` 保存被用户禁用的工具名（默认空 = 全部活跃）。
/// 禁用的工具既不进入模型上下文（[`Self::api_schemas`] 过滤），也会在
/// 执行时被拒绝（[`Self::dispatch`] fail-closed 兜底），双保险保证
/// 小模型即使不遵循上下文也无法触发被禁工具。
pub struct ToolRuntime {
    /// 保持注册序（api_schemas 下发顺序确定性；同名重注册原位替换，
    /// 对齐 Python dict 覆盖语义）。
    tools: Vec<Box<dyn Tool>>,
    index: HashMap<String, usize>,
    /// 被禁用的工具名集合（默认空 = 全部活跃）。Arc 共享：宿主装配时经
    /// [`Self::share_disabled`] 注入同一引用，/tools 面板修改后 Kernel
    /// 下一轮 api_schemas 即自动生效，无需跨会话通知。
    disabled: Arc<RwLock<HashSet<String>>>,
    /// 共享集合被修改时的通知回调（宿主注入）：ToolStateService 用它递增
    /// dirty 版本号，使 runtime 侧直写（[`Self::set_tool_enabled`] /
    /// [`Self::import_disabled`]）与面板侧修改统一记账，存储加载不会用
    /// 陈旧值覆盖未落盘修改（见 [`Self::share_disabled`]）。
    disabled_mutation_observer: Option<Arc<dyn Fn() + Send + Sync>>,
    permissions: Arc<PermissionEngine>,
    permission_prompt: Option<Arc<dyn PermissionPrompt>>,
    hooks: Option<Arc<HookExecutor>>,
    artifact_sink: Option<Arc<dyn ArtifactSink>>,
    /// 本轮查询的协作式取消标志（Kernel 工具批分发前注入、批后清除）。
    /// 经 [`Tool::set_query_cancel`] 下发给长时工具（shell 等）。
    query_cancel: Mutex<Option<Arc<AtomicBool>>>,
}

// 写锁内 observer 调用的重入哨兵状态（所有构建生效）：
// [`ToolRuntime::notify_disabled_mutated`] 在 disabled 写锁内调用 observer，
// 契约要求回调内不得访问 disabled 集合（RwLock 不可重入，回读会死锁）。
// 哨兵在回调执行区间置位，runtime 公开入口（`set_tool_enabled` /
// `is_tool_enabled` / `disabled_snapshot` / `import_disabled` /
// `api_schemas`）检测到置位即 panic——把"未来误用（如 metrics 钩子回读
// 集合）在线上静默死锁"转为立即 panic 暴露（review 建议 1：哨兵不随
// release 构建编译掉，死锁类缺陷必须在线上一出现即崩溃提示，而非挂起）。
// 检查成本为单次 TLS 读（纳秒级），仅在工具分发/注册表遍历路径上执行，
// 可忽略；observer 的正确用法（无锁记账，如原子计数器递增）不受影响。
thread_local! {
    static IN_OBSERVER_CALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII 复位哨兵：observer 正常返回或 panic 时均恢复置位前状态，避免异常
/// 路径把哨兵永久留在置位区间。
struct ObserverCallGuard;

impl Drop for ObserverCallGuard {
    fn drop(&mut self) {
        IN_OBSERVER_CALL.with(|flag| flag.set(false));
    }
}

/// 公开入口的哨兵检查：在 observer 回调区间内调用即 panic，提示违反
/// "回调内不得访问 disabled 集合"契约（review 中等问题 2 加固）。
fn assert_not_in_observer_call(entry: &str) {
    IN_OBSERVER_CALL.with(|flag| {
        assert!(
            !flag.get(),
            "{entry} called from within the disabled mutation observer: \
             observer runs inside the disabled write lock and must not touch \
             the disabled set (RwLock is not reentrant)"
        );
    });
}

impl Default for ToolRuntime {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            disabled: Arc::new(RwLock::new(HashSet::new())),
            disabled_mutation_observer: None,
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

    /// 设置工具活跃状态：`enabled=false` 时该工具不再进入模型上下文
    /// （[`Self::api_schemas`] 过滤）且执行被拒（[`Self::dispatch`]）。
    /// 默认所有工具活跃。仅在状态实际变化时通知共享集合的变更回调
    /// （宿主注入的 dirty 记账）。
    ///
    /// 单一事实源契约（review Nit）：宿主生产装配路径经
    /// [`Self::share_disabled`] 注入共享源后，状态读写统一由宿主层
    /// `ToolStateService`（`set_enabled` / `apply_from_store`）管理，本
    /// 方法不进入生产读写路径，保留用于框架独立使用与单元测试（observer
    /// 记账契约验证）。记账语义（仅在集合实际变化时通知）必须与
    /// `ToolStateService::set_enabled` 保持一致——修改其一须同步另一。
    pub fn set_tool_enabled(&self, name: &str, enabled: bool) {
        // 变更与 observer 记账在同一写锁临界区内（review 中等问题 2 修复）：
        // 消除"集合已变、dirty 未递增"的 check-then-act 窗口——并发加载
        // 持写锁检查版本号时，要么看到变更前的集合、要么看到已记账的版本
        // 号，不存在中间态。observer 仅做无锁记账（如原子 fetch_add），不得
        // 访问 disabled 集合（RwLock 不可重入，锁内回读会死锁）；重入哨兵
        // 把误用转为立即 panic（见 [`assert_not_in_observer_call`]）。
        assert_not_in_observer_call("set_tool_enabled");
        let mut guard = self.disabled.write().expect("tool disabled lock poisoned");
        let changed = if enabled {
            guard.remove(name)
        } else {
            guard.insert(name.to_string())
        };
        if changed {
            self.notify_disabled_mutated();
        }
    }

    /// 查询工具活跃状态（缺省 true）。
    pub fn is_tool_enabled(&self, name: &str) -> bool {
        assert_not_in_observer_call("is_tool_enabled");
        !self
            .disabled
            .read()
            .expect("tool disabled lock poisoned")
            .contains(name)
    }

    /// 共享禁用集合引用：宿主（/tools 面板）与 Kernel 的 ToolRuntime 经同一
    /// Arc 读写，面板变更在 Kernel 下一轮 api_schemas 自动生效。取回对称
    /// API（与 [`Self::share_disabled`] 对应）：生产装配经宿主层
    /// `ToolStateService::shared()` 注入，本方法保留用于框架独立使用/测试。
    pub fn disabled_source(&self) -> Arc<RwLock<HashSet<String>>> {
        Arc::clone(&self.disabled)
    }

    /// 以外部共享源替换内部禁用集合，并注入集合被修改时的通知回调：
    /// 宿主装配时传入 ToolStateService 的 dirty 递增回调（`Some`），使
    /// runtime 侧直写（[`Self::set_tool_enabled`]/[`Self::import_disabled`]）
    /// 与面板侧修改统一记账——存储加载（`apply_from_store`）不会因版本号
    /// 未递增而用陈旧值覆盖这些修改。无持久化记账语义的场景（如集成测试
    /// 仅验证共享源翻转）传 `None`。
    ///
    /// 回调契约（review 中等问题 2 修复）：observer 在 `disabled` 写锁**内**
    /// 调用——把记账移入临界区，消除"集合已变、dirty 未递增"的
    /// check-then-act 窗口（并发加载持写锁检查版本号时无中间态可观察）。
    /// 代价是回调内**不得访问 disabled 集合**：`RwLock` 不可重入，持锁
    /// 路径上回读会直接死锁；仅允许执行无锁记账（如原子计数器递增）。
    pub fn share_disabled(
        &mut self,
        source: Arc<RwLock<HashSet<String>>>,
        mutation_observer: Option<Arc<dyn Fn() + Send + Sync>>,
    ) {
        self.disabled = source;
        self.disabled_mutation_observer = mutation_observer;
    }

    /// 当前禁用集合快照（持久化用；框架独立使用 / 测试）。宿主生产路径的
    /// 持久化读经 `ToolStateService::disabled_snapshot`（同一共享源，语义
    /// 一致），本方法不进入生产读写路径（review Nit 单一事实源契约）。
    pub fn disabled_snapshot(&self) -> Vec<String> {
        assert_not_in_observer_call("disabled_snapshot");
        let guard = self.disabled.read().expect("tool disabled lock poisoned");
        let mut names: Vec<String> = guard.iter().cloned().collect();
        names.sort();
        names
    }

    /// 批量导入禁用集合（框架独立使用 / 测试）。集合实际内容变化时才
    /// 通知共享集合的变更回调（避免无意义的 dirty 递增）。
    ///
    /// 语义边界（review Nit）：本方法**无条件替换**集合并记账，不检查
    /// 宿主层的未落盘修改版本号——存储恢复路径必须走宿主层
    /// `ToolStateService::apply_from_store`（本地存在未落盘切换时跳过
    /// 陈旧值，fail-closed 保护），误用本方法恢复存储会覆盖用户刚做的
    /// 切换（破坏 fail-closed 语义）。
    pub fn import_disabled<I: IntoIterator<Item = String>>(&self, names: I) {
        // 同 [`Self::set_tool_enabled`]：observer 记账在写锁内完成（见
        // [`Self::share_disabled`] 的回调契约），重入哨兵同样生效。
        assert_not_in_observer_call("import_disabled");
        let mut guard = self.disabled.write().expect("tool disabled lock poisoned");
        // 集合相等判断（而非长度+包含）：重复输入（如 ["a","a"]）不应
        // 误报"已变化"而触发无意义的 dirty 记账（误判方向 fail-safe，但
        // 语义不严谨）。
        let incoming: HashSet<String> = names.into_iter().collect();
        let changed =
            incoming.len() != guard.len() || incoming.iter().any(|name| !guard.contains(name));
        guard.clear();
        guard.extend(incoming);
        if changed {
            self.notify_disabled_mutated();
        }
    }

    /// 通知共享集合被修改（宿主注入的 dirty 记账回调）。调用点在写锁**内**
    ///（见 [`Self::share_disabled`] 的回调契约）：observer 不得访问 disabled
    /// 集合（RwLock 不可重入），仅做无锁记账。回调执行期间重入哨兵置位，
    /// runtime 公开入口检测到即 panic——未来误用立即暴露而非线上死锁
    /// （review 中等问题 2 加固 + review 建议 1 全构建生效）。
    ///
    /// panic 隔离（review 建议 1 加固）：observer 在写锁内执行，若其 panic
    /// 穿过写锁 guard，RwLock 被 poison——之后所有工具入口
    /// （`is_tool_enabled` / `api_schemas` / `dispatch`）的 `.expect(...)`
    /// 都会 panic，整个工具系统永久瘫痪（含 Kernel 的 dispatch，中止
    /// agent loop）。用 `catch_unwind` 在写锁内拦截 observer panic：guard
    /// 正常 drop 不 poison，调用方（`set_tool_enabled` / `import_disabled`）
    /// 不感知异常。代价是本次记账可能缺失（dirty 未递增）——宿主的记账
    /// observer 自身不应 panic，此处仅防止宿主缺陷瘫痪工具系统，panic
    /// 语义降级为“该次变更未记账”，由存储加载的 dirty 保护兜底（宁缺
    /// 记账不瘫痪）。
    fn notify_disabled_mutated(&self) {
        if let Some(observer) = &self.disabled_mutation_observer {
            IN_OBSERVER_CALL.with(|flag| flag.set(true));
            let _guard = ObserverCallGuard;
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer()));
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
    /// 过滤被禁用的工具（非活跃工具不进入模型上下文）。
    pub fn api_schemas(&self) -> Vec<ToolDef> {
        assert_not_in_observer_call("api_schemas");
        // 先取禁用集合快照再遍历：`tool.definition()` 可能执行工具注册方
        // 代码，持读锁期间调用存在潜在死锁面（RwLock 非重入），以快照
        // 缩短持锁区间。
        let disabled: HashSet<String> = self
            .disabled
            .read()
            .expect("tool disabled lock poisoned")
            .clone();
        self.tools
            .iter()
            .filter_map(|tool| {
                let definition = tool.definition();
                (!disabled.contains(&definition.name) && tool.is_available()).then_some(definition)
            })
            .collect()
    }

    /// 全部常驻可用工具的 schema（不经过禁用过滤，保持注册序）——快照/面板展示用：
    /// 需包含已禁用的工具（面板卡片是重新启用的唯一入口），但不暴露宿主
    /// 一次性授权的临时工具，且不依赖"新建
    /// runtime 禁用集合为空"的隐式前提（`api_schemas` 过滤被禁工具；若快照
    /// 路径未来注入共享禁用源，已禁用工具会从面板消失且无法重新启用）。语义
    /// 与 [`Self::registered_names`] 一致（注册表全量视角），对比模型上下文
    /// 视角的 [`Self::api_schemas`]。
    pub fn all_schemas(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .filter(|tool| tool.is_available())
            .map(|tool| tool.definition())
            .collect()
    }

    /// 当前已注册的全部工具名（不经过禁用过滤）——宿主注册契约断言与调试
    /// 用。`api_schemas` 过滤被禁用的工具（模型上下文视角），本方法返回原始
    /// 注册集，语义与落盘过滤用的已知工具集（宿主 REGISTERED_TOOL_NAMES
    /// 缓存）一致；契约断言若用带过滤的 `api_schemas` 比较，禁用集合非空时
    /// 必然不一致而误报（见 web service.rs `register_tools`）。
    pub fn registered_names(&self) -> HashSet<String> {
        self.tools
            .iter()
            .map(|tool| tool.definition().name)
            .collect()
    }

    /// 单个 tool_use 的完整分发管线。未知工具与执行异常均归一化为
    /// is_error 的 ToolResult（对齐基线合成 error tool_result 语义）。
    pub async fn dispatch(&self, tool_use: &ToolUse, ctx: &mut ToolContext<'_>) -> ToolResult {
        // 0. Stop may arrive while another tool in this batch is awaiting a
        //    permission reply.  Never start a later hook/prompt/tool after it;
        //    the Kernel remains responsible for consuming the flag at the batch
        //    boundary and reporting QUERY_INTERRUPTED_STATUS.
        if self.query_is_cancelled() {
            return self.apply_output_budget(tool_use, Self::cancelled_result(), ctx);
        }
        // 1. 活跃状态检查（fail-closed）：禁用的工具即使被模型输出也拒绝执行。
        //    置于 hooks 之前，避免为已禁用的工具触发 hook 副作用。
        if !self.is_tool_enabled(&tool_use.name) {
            let result = ToolResult::err(format!(
                "Tool '{}' is disabled and cannot be called",
                tool_use.name
            ));
            return self.apply_output_budget(tool_use, result, ctx);
        }
        // 2. pre_tool_use hook：任一 blocked 即拒绝执行
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

        // 3. 工具解析
        let Some(tool) = self.get(&tool_use.name) else {
            let result = ToolResult::err(format!("Unknown tool: {}", tool_use.name));
            return self.apply_output_budget(tool_use, result, ctx);
        };
        if !tool.is_available() {
            let result = ToolResult::err(format!(
                "Tool '{}' is not authorized for the current request",
                tool_use.name
            ));
            return self.apply_output_budget(tool_use, result, ctx);
        }

        // 4. 三态权限：路径/命令归一化后求值（对齐 _resolve_permission_file_path）
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

        // 5. 执行（Err 归一化为 is_error tool_result）。先注入查询级取消标志
        //    （含 None 清除旧值），长时工具据此响应 UI 中断。
        if self.query_is_cancelled() {
            return self.apply_output_budget(tool_use, Self::cancelled_result(), ctx);
        }
        tool.set_query_cancel(self.current_query_cancel());
        let result = match tool.execute(tool_use.input.clone(), ctx).await {
            Ok(result) => result,
            Err(error) => ToolResult::err(format!("Tool {} failed: {error}", tool_use.name)),
        };

        // 6. 输出预算：超长外置 + 内联预览
        let result = self.apply_output_budget(tool_use, result, ctx);

        // 7. post_tool_use hook：观察性执行，结果不改写 tool_result（对齐基线）
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
    async fn disabled_tool_excluded_from_api_schemas() {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "keep",
            read_only: true,
        }));
        runtime.register(Box::new(EchoTool {
            name: "drop",
            read_only: true,
        }));
        // 默认全部活跃
        assert!(runtime.is_tool_enabled("drop"));
        runtime.set_tool_enabled("drop", false);
        assert!(!runtime.is_tool_enabled("drop"));
        let names: Vec<String> = runtime
            .api_schemas()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert_eq!(
            names,
            vec!["keep"],
            "disabled tool must be filtered from model context"
        );
        // 重新启用恢复
        runtime.set_tool_enabled("drop", true);
        assert!(runtime.is_tool_enabled("drop"));
        let names: Vec<String> = runtime
            .api_schemas()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert_eq!(names, vec!["keep", "drop"]);
    }

    #[test]
    fn registered_names_ignores_disabled_filter() {
        // 契约（review Nit 1）：registered_names 返回无过滤的原始注册集——
        // 宿主用它校验 REGISTERED_TOOL_NAMES 缓存一致性。若带禁用过滤，
        // 断言在禁用集合非空时误报 panic，掩盖真实缓存失效（此前 web 侧
        // 用 api_schemas 比较正是如此，见 service.rs register_tools）。
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "keep",
            read_only: true,
        }));
        runtime.register(Box::new(EchoTool {
            name: "drop",
            read_only: true,
        }));
        runtime.set_tool_enabled("drop", false);
        assert_eq!(
            runtime.api_schemas().len(),
            1,
            "model context must be filtered"
        );
        let names = runtime.registered_names();
        assert!(names.contains("keep"));
        assert!(
            names.contains("drop"),
            "registered_names must ignore the disabled filter"
        );
    }

    #[tokio::test]
    async fn all_schemas_includes_disabled_tools_and_keeps_order() {
        // 契约（review 建议 4）：all_schemas 是注册表全量视角（无禁用过滤、
        // 保持注册序），与模型上下文视角的 api_schemas 相对——面板/快照需
        // 展示已禁用工具（卡片是重新启用的唯一入口），且不依赖"新建 runtime
        // 禁用集合为空"的隐式前提（若未来注入共享禁用源，api_schemas 过滤
        // 会让已禁用工具从面板消失且无法重新启用）。
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "keep",
            read_only: true,
        }));
        runtime.register(Box::new(EchoTool {
            name: "drop",
            read_only: true,
        }));
        runtime.set_tool_enabled("drop", false);
        // 模型上下文视角：过滤被禁工具
        assert_eq!(runtime.api_schemas().len(), 1);
        // 注册表全量视角：包含被禁工具且保持注册序
        let names: Vec<String> = runtime
            .all_schemas()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert_eq!(
            names,
            vec!["keep", "drop"],
            "all_schemas must include disabled tools and keep registration order"
        );
    }

    #[tokio::test]
    async fn disabled_tool_dispatch_is_rejected_fail_closed() {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "blocked",
            read_only: true,
        }));
        runtime.set_tool_enabled("blocked", false);
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(
                &tool_use("blocked", serde_json::json!({"text": "echo-leak-marker"})),
                &mut ctx,
            )
            .await;
        assert!(result.is_error, "disabled tool dispatch must fail closed");
        assert!(result.output.contains("disabled"), "{}:", result.output);
        // 禁用工具不得触发执行：若 EchoTool 被误执行，输出会包含回显的
        // "echo-leak-marker"，此处断言即真实拦截副作用泄露。
        assert!(
            !result.output.contains("echo-leak-marker"),
            "side effect leaked: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn cancellation_takes_precedence_over_disabled_check() {
        // dispatch 管线顺序（review 建议补测）：查询级取消检查位于活跃状态
        // 检查之前——用户按下 Stop 后，即便工具已被禁用也返回统一的
        // "interrupted" 结果，Kernel 在批边界消费取消标志并上报
        // QUERY_INTERRUPTED_STATUS。若顺序颠倒（禁用优先），取消语义会被
        // 禁用结果掩盖：UI 无法区分"用户主动中断"与"工具被禁"，且取消
        // 后的后续批内工具会继续执行（破坏 Stop 语义）。
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "blocked",
            read_only: true,
        }));
        runtime.set_tool_enabled("blocked", false);
        assert!(!runtime.is_tool_enabled("blocked"));
        runtime.set_query_cancel(Some(Arc::new(AtomicBool::new(true))));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = runtime
            .dispatch(&tool_use("blocked", Value::Null), &mut ctx)
            .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("interrupted"),
            "cancellation must take precedence over the disabled check: {}",
            result.output
        );
        assert!(
            !result.output.contains("disabled"),
            "cancelled result must not be masked by the disabled message: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn import_disabled_and_snapshot_roundtrip() {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(EchoTool {
            name: "a",
            read_only: true,
        }));
        runtime.register(Box::new(EchoTool {
            name: "b",
            read_only: true,
        }));
        runtime.import_disabled(["a".to_string(), "b".to_string()]);
        assert!(!runtime.is_tool_enabled("a"));
        assert!(!runtime.is_tool_enabled("b"));
        assert_eq!(
            runtime.disabled_snapshot(),
            vec!["a".to_string(), "b".to_string()]
        );
        // 快照返回禁用名集合；清空后全部恢复
        runtime.import_disabled(Vec::<String>::new());
        assert!(runtime.is_tool_enabled("a"));
        assert!(runtime.is_tool_enabled("b"));
        assert!(runtime.disabled_snapshot().is_empty());
    }

    #[tokio::test]
    async fn shared_disabled_source_reflects_external_mutation() {
        let runtime = ToolRuntime::new();
        let source = runtime.disabled_source();
        // 宿主（面板）持有共享引用直接修改，runtime 下一轮立即感知
        source.write().expect("lock").insert("external".to_string());
        assert!(!runtime.is_tool_enabled("external"));
    }

    #[tokio::test]
    async fn shared_disabled_observer_notifies_only_on_actual_mutation() {
        // 修复回归（review P3-1）：runtime 侧直写共享集合必须经注入的
        // observer 统一记账——否则宿主经 ToolStateService 持久化路径判断
        // 的 dirty 版本号不递增，存储加载会用陈旧值覆盖直写修改。
        let mut runtime = ToolRuntime::new();
        let source: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        let notified = Arc::new(AtomicUsize::new(0));
        let observer = Arc::new({
            let notified = Arc::clone(&notified);
            move || {
                notified.fetch_add(1, Ordering::SeqCst);
            }
        });
        runtime.share_disabled(source, Some(observer));

        // 实际变化才通知：与 ToolStateService::set_enabled 递增语义一致
        runtime.set_tool_enabled("a", false);
        assert_eq!(notified.load(Ordering::SeqCst), 1);
        runtime.set_tool_enabled("a", false);
        assert_eq!(
            notified.load(Ordering::SeqCst),
            1,
            "no-op disable must not notify"
        );
        runtime.set_tool_enabled("a", true);
        assert_eq!(notified.load(Ordering::SeqCst), 2);

        // import_disabled 全量替换：内容变化才通知
        runtime.import_disabled(["b".to_string()]);
        assert_eq!(notified.load(Ordering::SeqCst), 3);
        runtime.import_disabled(["b".to_string()]);
        assert_eq!(
            notified.load(Ordering::SeqCst),
            3,
            "identical import must not notify"
        );
        // 外部直写共享 Arc（宿主面板等价物）不经 runtime 方法，不触发回调
        // ——那是 ToolStateService::set_enabled 自己的记账职责。
        runtime
            .disabled
            .write()
            .expect("lock")
            .insert("c".to_string());
        assert_eq!(notified.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn observer_call_scope_is_restored_after_notify() {
        // 契约（review 中等问题 2 加固）：observer 在 disabled 写锁内执行，
        // 重入哨兵在回调执行区间置位；回调返回后必须复位（RAII guard），
        // 否则后续正常调用会被误判为重入而 panic。哨兵不随构建配置编译掉
        // （review 建议 1），本测试无条件运行。
        let mut runtime = ToolRuntime::new();
        let source: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        let observed = Arc::new(AtomicBool::new(false));
        let observer = Arc::new({
            let observed = Arc::clone(&observed);
            move || {
                observed.store(
                    super::IN_OBSERVER_CALL.with(|flag| flag.get()),
                    Ordering::SeqCst,
                );
            }
        });
        runtime.share_disabled(source, Some(observer));

        runtime.set_tool_enabled("a", false);
        assert!(
            observed.load(Ordering::SeqCst),
            "observer must run inside the reentry scope"
        );
        assert!(
            !super::IN_OBSERVER_CALL.with(|flag| flag.get()),
            "scope must be restored after notify (RAII guard)"
        );
        // 复位后正常入口不受影响（哨兵无残留）
        assert!(!runtime.is_tool_enabled("a"));
    }

    #[test]
    fn read_entrypoints_panic_inside_observer_call_scope() {
        // 契约（review 中等问题 2 加固）：observer 在 disabled 写锁内执行，
        // 回调内回读集合会死锁——重入哨兵（所有构建生效，review 建议 1）
        // 把置位区间内的公开入口调用转为立即 panic，未来误用（如 metrics
        // 钩子回读集合）一出现即崩溃提示，而非线上静默死锁。逐个验证读锁
        // 入口（含 dispatch 依赖的 is_tool_enabled）。
        for entry in [
            "is_tool_enabled",
            "disabled_snapshot",
            "api_schemas",
            "set_tool_enabled",
            "import_disabled",
        ] {
            super::IN_OBSERVER_CALL.with(|flag| flag.set(true));
            let result = std::panic::catch_unwind(|| {
                let runtime = ToolRuntime::new();
                match entry {
                    "is_tool_enabled" => {
                        let _ = runtime.is_tool_enabled("a");
                    }
                    "disabled_snapshot" => {
                        let _ = runtime.disabled_snapshot();
                    }
                    "api_schemas" => {
                        let _ = runtime.api_schemas();
                    }
                    "set_tool_enabled" => {
                        runtime.set_tool_enabled("a", false);
                    }
                    "import_disabled" => {
                        runtime.import_disabled(["a".to_string()]);
                    }
                    _ => unreachable!(),
                }
            });
            super::IN_OBSERVER_CALL.with(|flag| flag.set(false));
            assert!(
                result.is_err(),
                "{entry} must panic when called from within the observer call scope"
            );
        }
    }

    #[test]
    fn observer_panic_is_contained_and_tool_system_stays_usable() {
        // 修复（review 建议 1）：observer 在 disabled 写锁内执行，若其 panic
        // 穿过写锁 guard 会 poison 锁——之后 is_tool_enabled / api_schemas /
        // dispatch 的 `.expect(...)` 全部 panic，整个工具系统永久瘫痪。
        // catch_unwind 在写锁内拦截后：guard 正常 drop 不 poison，集合修改
        // 生效（observer panic 发生在记账前，但集合已变），后续调用全部
        // 正常；哨兵由 RAII guard 复位，不残留置位区间。
        let mut runtime = ToolRuntime::new();
        let source: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        let observer = Arc::new(|| panic!("observer exploded"));
        runtime.share_disabled(source, Some(observer));

        // 调用不传播 panic（notify 内部 catch_unwind 拦截）
        runtime.set_tool_enabled("a", false);
        // 集合修改生效
        assert!(!runtime.is_tool_enabled("a"));
        // 锁未 poison：后续写/读入口全部正常
        runtime.set_tool_enabled("a", true);
        assert!(runtime.is_tool_enabled("a"));
        runtime.import_disabled(["b".to_string()]);
        assert!(!runtime.is_tool_enabled("b"));
        assert!(runtime.api_schemas().is_empty());
        // 哨兵复位：不在 observer 调用区间
        assert!(
            !super::IN_OBSERVER_CALL.with(|flag| flag.get()),
            "sentinel must be restored after observer panic (RAII guard)"
        );
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
