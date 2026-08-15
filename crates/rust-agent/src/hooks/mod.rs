//! Hook System（对齐 Harness `hooks/`：events / types / schemas / executor）。
//!
//! Phase 3 范围：10 个触发点 + command / prompt 两类定义先行（http / agent
//! 后置，schema 变体已预留判别式但不注册执行器）；matcher（fnmatch）、
//! priority（高优先先行、同级保持注册序）、block_on_failure、
//! AggregatedHookResult 任一 blocked 即阻断。
//!
//! command hook 仅 Native 端可执行（进程派生）；WASM 端返回失败结果
//! （blocked 依 block_on_failure），不 panic 不阻断其余 hook。

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::fnmatch::fnmatch;
use crate::kernel::messages::ConversationMessage;
use crate::model_client::{ModelClient, ModelRequest, ModelStreamEvent};
#[cfg(not(target_arch = "wasm32"))]
use crate::policy::sandbox::ShellRequest;
use crate::policy::sandbox::{NoopSandbox, Sandbox};
use crate::prompts::PROMPT_HOOK_SYSTEM_PROMPT;

/// Hook 触发点（对齐 `hooks/events.py`，10 个）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Notification,
    Stop,
    SubagentStop,
}

impl HookEvent {
    /// 事件名字符串（payload `event` 字段与 hook 环境变量用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::Notification => "notification",
            Self::Stop => "stop",
            Self::SubagentStop => "subagent_stop",
        }
    }
}

fn default_timeout_seconds() -> u32 {
    30
}

fn default_block_on_failure_prompt() -> bool {
    true
}

/// command hook 的 stdout/stderr 各自硬上限，避免项目 hook 通过无界输出
/// 耗尽宿主内存。最终 Tool/Hook 文本预算不能替代进程管道读取上限。
#[cfg(not(target_arch = "wasm32"))]
const COMMAND_HOOK_STREAM_MAX_BYTES: usize = 256 * 1024;
/// 所有 hook 的结构化 payload 上限，以及 prompt hook 的模型请求/响应上限。
/// `max_output_tokens` 只是对 ModelClient 的协议请求，不作为可信的内存边界。
const HOOK_PAYLOAD_MAX_BYTES: usize = 256 * 1024;
const PROMPT_HOOK_MESSAGE_MAX_BYTES: usize = 256 * 1024;

/// command hook：执行 shell 命令，退出码非 0 视为失败
/// （对齐 `CommandHookDefinition`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandHookDefinition {
    pub command: String,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u32,
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub block_on_failure: bool,
    /// 高优先先行；同级保持注册序。
    #[serde(default)]
    pub priority: i32,
}

/// prompt hook：请模型校验条件，返回严格 JSON `{"ok": bool, "reason": …}`
/// （对齐 `PromptHookDefinition`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptHookDefinition {
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u32,
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default = "default_block_on_failure_prompt")]
    pub block_on_failure: bool,
    #[serde(default)]
    pub priority: i32,
}

/// Hook 定义判别式（`type` 字段；http / agent 两类 Phase 3 后置）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookDefinition {
    Command(CommandHookDefinition),
    Prompt(PromptHookDefinition),
}

impl HookDefinition {
    fn priority(&self) -> i32 {
        match self {
            Self::Command(hook) => hook.priority,
            Self::Prompt(hook) => hook.priority,
        }
    }

    fn matcher(&self) -> Option<&str> {
        match self {
            Self::Command(hook) => hook.matcher.as_deref(),
            Self::Prompt(hook) => hook.matcher.as_deref(),
        }
    }
}

/// 单个 hook 的执行结果（对齐 `hooks/types.py::HookResult`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookResult {
    pub hook_type: String,
    pub success: bool,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

/// 单事件的聚合结果：任一 blocked 即阻断（对齐 `AggregatedHookResult`）。
#[derive(Debug, Clone, Default)]
pub struct AggregatedHookResult {
    pub results: Vec<HookResult>,
}

impl AggregatedHookResult {
    pub fn blocked(&self) -> bool {
        self.results.iter().any(|result| result.blocked)
    }

    /// 首个阻断原因（reason 为空回落 output）。
    pub fn reason(&self) -> String {
        self.results
            .iter()
            .find(|result| result.blocked)
            .map(|result| {
                if result.reason.is_empty() {
                    result.output.clone()
                } else {
                    result.reason.clone()
                }
            })
            .unwrap_or_default()
    }
}

/// Hook 注册表：事件 → 定义列表，按 priority 降序稳定排序
/// （高优先先行；同级保持注册序）。
#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    hooks: Vec<(HookEvent, HookDefinition)>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, event: HookEvent, definition: HookDefinition) {
        self.hooks.push((event, definition));
    }

    /// 仅在同一事件下尚无完全相同定义时注册。常规 [`Self::register`]
    /// 保留允许重复 hook 的原有语义；插件重载则应使用本方法，避免每次
    /// 注入都让相同 command hook 额外执行一次。
    pub fn register_if_absent(&mut self, event: HookEvent, definition: HookDefinition) -> bool {
        if self.hooks.iter().any(|(registered_event, registered)| {
            *registered_event == event && registered == &definition
        }) {
            false
        } else {
            self.register(event, definition);
            true
        }
    }

    /// 返回某事件的定义（priority 降序，稳定）。
    pub fn get(&self, event: HookEvent) -> Vec<&HookDefinition> {
        let mut matched: Vec<&HookDefinition> = self
            .hooks
            .iter()
            .filter(|(hook_event, _)| *hook_event == event)
            .map(|(_, definition)| definition)
            .collect();
        // sort_by_key 为稳定排序：同 priority 保持注册序
        matched.sort_by_key(|definition| std::cmp::Reverse(definition.priority()));
        matched
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

/// Hook 执行引擎（对齐 `hooks/executor.py::HookExecutor`）。
pub struct HookExecutor {
    /// 注册表经 RwLock 内部可变：宿主持 `Arc<HookExecutor>` 时仍可热更新
    /// （对齐基线 `update_registry` 语义；hot reload 由宿主驱动）。
    registry: std::sync::RwLock<HookRegistry>,
    /// command hook 的工作目录（仅 Native 消费；WASM 无进程派生）。
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    cwd: PathBuf,
    /// prompt hook 的模型通道；未注入时 prompt hook 报失败。
    model: Option<Arc<dyn ModelClient>>,
    default_model: Option<String>,
    /// command hook 与 shell tool 共用同一强制隔离关口。默认 Noop，避免宿主
    /// 忘记注入平台沙箱时静默退化为宿主进程执行。
    sandbox: Arc<dyn Sandbox>,
}

impl HookExecutor {
    pub fn new(registry: HookRegistry, cwd: PathBuf) -> Self {
        Self {
            registry: std::sync::RwLock::new(registry),
            cwd,
            model: None,
            default_model: None,
            sandbox: Arc::new(NoopSandbox),
        }
    }

    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_model(
        mut self,
        model: Arc<dyn ModelClient>,
        default_model: Option<String>,
    ) -> Self {
        self.model = Some(model);
        self.default_model = default_model;
        self
    }

    /// 替换活动注册表（对齐 `update_registry`；`&self` 内部可变，
    /// 支持 `Arc<HookExecutor>` 宿主热重载）。
    pub fn update_registry(&self, registry: HookRegistry) {
        *self.registry.write().expect("hook registry lock poisoned") = registry;
    }

    /// 执行某事件的全部命中 hook，逐个聚合（单 hook 失败不中断其余）。
    pub async fn execute(
        &self,
        event: HookEvent,
        payload: &Map<String, Value>,
    ) -> AggregatedHookResult {
        // 先快照命中的定义再异步执行：不得跨 await 持有 RwLock 读锁
        let matched: Vec<HookDefinition> = {
            let registry = self.registry.read().expect("hook registry lock poisoned");
            registry
                .get(event)
                .into_iter()
                .filter(|definition| matches_hook(definition, payload))
                .cloned()
                .collect()
        };
        let mut results = Vec::new();
        for definition in &matched {
            let result = match definition {
                HookDefinition::Command(hook) => self.run_command_hook(hook, event, payload).await,
                HookDefinition::Prompt(hook) => self.run_prompt_hook(hook, payload).await,
            };
            results.push(result);
        }
        AggregatedHookResult { results }
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn run_command_hook(
        &self,
        hook: &CommandHookDefinition,
        event: HookEvent,
        payload: &Map<String, Value>,
    ) -> HookResult {
        use std::time::Duration;

        let serialized = match serialize_hook_payload(payload) {
            Ok(serialized) => serialized,
            Err(()) => {
                return HookResult {
                    hook_type: "command".into(),
                    success: false,
                    blocked: hook.block_on_failure,
                    reason: format!(
                        "command hook payload exceeded the {HOOK_PAYLOAD_MAX_BYTES} byte limit"
                    ),
                    ..Default::default()
                };
            }
        };
        let timeout = Duration::from_secs(u64::from(hook.timeout_seconds));
        if !self.sandbox.capabilities().shell {
            return HookResult {
                hook_type: "command".into(),
                success: false,
                blocked: hook.block_on_failure,
                reason: format!(
                    "command hook requires shell capability, but sandbox '{}' does not provide it",
                    self.sandbox.name()
                ),
                ..Default::default()
            };
        }
        let injected = inject_arguments(&hook.command, &serialized, true);
        let command = format!(
            "export AINS_HOOK_EVENT={}; export AINS_HOOK_PAYLOAD={}; {injected}",
            shell_quote(event.as_str()),
            shell_quote(&serialized)
        );
        let request = ShellRequest {
            command,
            cwd: self.cwd.clone(),
            timeout,
            max_output_bytes: COMMAND_HOOK_STREAM_MAX_BYTES,
            cancel: None,
            output_sink: None,
        };
        // 不经外层重复 timeout：Sandbox 契约要求后端强制 request.timeout，
        // 外层包装会在竞态窗口中抢先 drop future，只杀包装进程而绕过后端
        // 的进程树终止（killpg/kill-on-close），导致命令残留。
        let outcome = match self.sandbox.exec_shell(request).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return HookResult {
                    hook_type: "command".into(),
                    success: false,
                    blocked: hook.block_on_failure,
                    reason: error.to_string(),
                    ..Default::default()
                };
            }
        };
        if outcome.timed_out {
            return HookResult {
                hook_type: "command".into(),
                success: false,
                blocked: hook.block_on_failure,
                reason: format!("command hook timed out after {}s", hook.timeout_seconds),
                ..Default::default()
            };
        }
        let (combined, truncated) = truncate_hook_output(outcome.output.trim());
        let returncode = outcome.exit_code;
        let success = returncode == Some(0);
        let mut metadata = Map::new();
        metadata.insert("returncode".into(), serde_json::json!(returncode));
        metadata.insert("output_truncated".into(), Value::Bool(truncated));
        HookResult {
            hook_type: "command".into(),
            success,
            blocked: hook.block_on_failure && !success,
            // 成功时 reason 置空（review 修复：失败语义字段不应在成功时携带输出）
            reason: if success {
                String::new()
            } else if combined.is_empty() {
                format!(
                    "command hook failed with exit code {}",
                    returncode.map_or_else(|| "unknown".to_string(), |code| code.to_string())
                )
            } else {
                combined.clone()
            },
            output: combined,
            metadata,
        }
    }

    /// WASM 端无进程派生能力：command hook 报失败（blocked 依 block_on_failure）。
    #[cfg(target_arch = "wasm32")]
    async fn run_command_hook(
        &self,
        hook: &CommandHookDefinition,
        _event: HookEvent,
        _payload: &Map<String, Value>,
    ) -> HookResult {
        HookResult {
            hook_type: "command".into(),
            success: false,
            blocked: hook.block_on_failure,
            reason: "command hooks are not supported on the web platform".into(),
            ..Default::default()
        }
    }

    async fn run_prompt_hook(
        &self,
        hook: &PromptHookDefinition,
        payload: &Map<String, Value>,
    ) -> HookResult {
        let Some(model) = &self.model else {
            return HookResult {
                hook_type: "prompt".into(),
                success: false,
                blocked: hook.block_on_failure,
                reason: "prompt hooks require a model client".into(),
                ..Default::default()
            };
        };

        let serialized = match serialize_hook_payload(payload) {
            Ok(serialized) => serialized,
            Err(()) => {
                return HookResult {
                    hook_type: "prompt".into(),
                    success: false,
                    blocked: hook.block_on_failure,
                    reason: format!(
                        "prompt hook payload exceeded the {HOOK_PAYLOAD_MAX_BYTES} byte limit"
                    ),
                    ..Default::default()
                };
            }
        };
        let prompt = inject_arguments(&hook.prompt, &serialized, false);
        if prompt.len() > PROMPT_HOOK_MESSAGE_MAX_BYTES {
            return HookResult {
                hook_type: "prompt".into(),
                success: false,
                blocked: hook.block_on_failure,
                reason: format!(
                    "prompt hook request exceeded the {PROMPT_HOOK_MESSAGE_MAX_BYTES} byte limit"
                ),
                ..Default::default()
            };
        }
        let request = ModelRequest {
            model: hook.model.clone().or_else(|| self.default_model.clone()),
            messages: vec![ConversationMessage::from_user_text(prompt)],
            system_prompt: Some(PROMPT_HOOK_SYSTEM_PROMPT.to_string()),
            max_output_tokens: 512,
            tools: Vec::new(),
        };

        let block_on_failure = hook.block_on_failure;
        let run = async move {
            let mut stream = match model.stream_response(request).await {
                Ok(stream) => stream,
                Err(error) => {
                    return HookResult {
                        hook_type: "prompt".into(),
                        success: false,
                        blocked: block_on_failure,
                        reason: error.to_string(),
                        ..Default::default()
                    };
                }
            };
            let mut deltas = String::new();
            let mut final_text: Option<String> = None;
            while let Some(event) = stream.next().await {
                match event {
                    ModelStreamEvent::TextDelta { text } => {
                        let exceeds_limit = deltas
                            .len()
                            .checked_add(text.len())
                            .is_none_or(|size| size > PROMPT_HOOK_MESSAGE_MAX_BYTES);
                        if exceeds_limit {
                            return HookResult {
                                hook_type: "prompt".into(),
                                success: false,
                                blocked: block_on_failure,
                                reason: format!(
                                    "prompt hook response exceeded the {PROMPT_HOOK_MESSAGE_MAX_BYTES} byte limit"
                                ),
                                ..Default::default()
                            };
                        }
                        deltas.push_str(&text);
                    }
                    ModelStreamEvent::Complete { message, .. } => {
                        let text = message.text();
                        if text.len() > PROMPT_HOOK_MESSAGE_MAX_BYTES {
                            return HookResult {
                                hook_type: "prompt".into(),
                                success: false,
                                blocked: block_on_failure,
                                reason: format!(
                                    "prompt hook response exceeded the {PROMPT_HOOK_MESSAGE_MAX_BYTES} byte limit"
                                ),
                                ..Default::default()
                            };
                        }
                        if !text.is_empty() {
                            final_text = Some(text);
                        }
                    }
                    ModelStreamEvent::Retry { .. } => {}
                }
            }
            let text = final_text.unwrap_or(deltas);

            let (ok, reason) = parse_hook_json(&text);
            if ok {
                HookResult {
                    hook_type: "prompt".into(),
                    success: true,
                    output: text,
                    ..Default::default()
                }
            } else {
                HookResult {
                    hook_type: "prompt".into(),
                    success: false,
                    output: text,
                    blocked: block_on_failure,
                    reason,
                    ..Default::default()
                }
            }
        };

        let timeout = std::time::Duration::from_secs(u64::from(hook.timeout_seconds));
        match run_prompt_with_timeout(timeout, run).await {
            Ok(result) => result,
            Err(()) => HookResult {
                hook_type: "prompt".into(),
                success: false,
                blocked: hook.block_on_failure,
                reason: format!("prompt hook timed out after {}s", hook.timeout_seconds),
                ..Default::default()
            },
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn truncate_hook_output(output: &str) -> (String, bool) {
    if output.len() <= COMMAND_HOOK_STREAM_MAX_BYTES {
        return (output.to_string(), false);
    }
    let mut end = COMMAND_HOOK_STREAM_MAX_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!("{}\n...[output truncated]...", &output[..end]),
        true,
    )
}

#[cfg(not(target_arch = "wasm32"))]
async fn run_prompt_with_timeout<F>(
    duration: std::time::Duration,
    future: F,
) -> Result<HookResult, ()>
where
    F: std::future::Future<Output = HookResult>,
{
    tokio::time::timeout(duration, future).await.map_err(|_| ())
}

#[cfg(target_arch = "wasm32")]
async fn run_prompt_with_timeout<F>(
    duration: std::time::Duration,
    future: F,
) -> Result<HookResult, ()>
where
    F: std::future::Future<Output = HookResult>,
{
    use crate::runtime_adapter::RuntimeAdapter;
    use futures::future::{Either, select};

    match select(
        Box::pin(future),
        Box::pin(crate::WasmRuntimeAdapter::sleep(duration)),
    )
    .await
    {
        Either::Left((result, _)) => Ok(result),
        Either::Right(((), _)) => Err(()),
    }
}

/// matcher 匹配（对齐 `_matches_hook`）：无 matcher 恒命中；否则以
/// payload 的 tool_name → prompt → event 首个非空字段做 fnmatch。
fn matches_hook(definition: &HookDefinition, payload: &Map<String, Value>) -> bool {
    let Some(matcher) = definition.matcher() else {
        return true;
    };
    if matcher.is_empty() {
        return true;
    }
    let subject = ["tool_name", "prompt", "event"]
        .iter()
        .find_map(|key| {
            payload
                .get(*key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("");
    fnmatch(subject, matcher)
}

/// `$ARGUMENTS` 注入（对齐 `_inject_arguments`）：payload JSON 序列化后替换；
/// command hook 场景做 shell 单引号转义。
fn inject_arguments(template: &str, serialized_payload: &str, shell_escape: bool) -> String {
    let replacement = if shell_escape {
        shell_quote(serialized_payload)
    } else {
        serialized_payload.to_string()
    };
    template.replace("$ARGUMENTS", &replacement)
}

/// Serialize hook input through a bounded writer so rejecting an oversized
/// payload does not first require an equally oversized temporary String.
fn serialize_hook_payload(payload: &Map<String, Value>) -> Result<String, ()> {
    struct LimitedBuffer {
        bytes: Vec<u8>,
    }

    impl std::io::Write for LimitedBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
                return Err(std::io::Error::other("hook payload size overflow"));
            };
            if next_len > HOOK_PAYLOAD_MAX_BYTES {
                return Err(std::io::Error::other("hook payload limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut buffer = LimitedBuffer { bytes: Vec::new() };
    serde_json::to_writer(&mut buffer, payload).map_err(|_| ())?;
    String::from_utf8(buffer.bytes).map_err(|_| ())
}

/// POSIX shell 单引号安全转义（等价 Python `shlex.quote`）。
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// 宽松解析 hook 模型答复（对齐 `_parse_hook_json`）：严格 JSON `{ok, reason}`
/// 优先；否则 `ok/true/yes` 视为通过；其余视为拒绝并把全文作为 reason。
fn parse_hook_json(text: &str) -> (bool, String) {
    if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(text)
        && let Some(ok) = parsed.get("ok").and_then(Value::as_bool)
    {
        let reason = parsed
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("hook rejected the event")
            .to_string();
        return (ok, if ok { String::new() } else { reason });
    }
    let lowered = text.trim().to_lowercase();
    if matches!(lowered.as_str(), "ok" | "true" | "yes") {
        return (true, String::new());
    }
    let reason = text.trim();
    (
        false,
        if reason.is_empty() {
            "hook returned invalid JSON".to_string()
        } else {
            reason.to_string()
        },
    )
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::kernel::ScriptedModelClient;
    use crate::model_client::UsageSnapshot;
    use crate::policy::sandbox::{SandboxCapabilities, SandboxError, ShellOutcome};

    struct TestShellSandbox;

    #[async_trait::async_trait]
    impl Sandbox for TestShellSandbox {
        fn name(&self) -> &'static str {
            "test-shell"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                shell: true,
                ..Default::default()
            }
        }

        async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
            use std::process::Stdio;

            assert_eq!(request.max_output_bytes, COMMAND_HOOK_STREAM_MAX_BYTES);
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(request.command)
                .current_dir(request.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| SandboxError::Execution(error.to_string()))?;
            match tokio::time::timeout(request.timeout, child.wait_with_output()).await {
                Ok(Ok(output)) => {
                    let mut combined = output.stdout;
                    combined.extend(output.stderr);
                    Ok(ShellOutcome {
                        output: String::from_utf8_lossy(&combined).into_owned(),
                        exit_code: output.status.code(),
                        timed_out: false,
                        cancelled: false,
                    })
                }
                Ok(Err(error)) => Err(SandboxError::Execution(error.to_string())),
                Err(_) => Ok(ShellOutcome {
                    output: String::new(),
                    exit_code: None,
                    timed_out: true,
                    cancelled: false,
                }),
            }
        }
    }

    fn command_executor(registry: HookRegistry) -> HookExecutor {
        HookExecutor::new(registry, std::env::temp_dir()).with_sandbox(Arc::new(TestShellSandbox))
    }

    fn command_hook(command: &str, priority: i32, block: bool) -> HookDefinition {
        HookDefinition::Command(CommandHookDefinition {
            command: command.into(),
            timeout_seconds: 10,
            matcher: None,
            block_on_failure: block,
            priority,
        })
    }

    #[test]
    fn registry_orders_by_priority_then_registration() {
        let mut registry = HookRegistry::new();
        registry.register(HookEvent::PreToolUse, command_hook("echo a", 0, false));
        registry.register(HookEvent::PreToolUse, command_hook("echo b", 5, false));
        registry.register(HookEvent::PreToolUse, command_hook("echo c", 0, false));
        registry.register(HookEvent::Stop, command_hook("echo other", 9, false));
        let ordered: Vec<String> = registry
            .get(HookEvent::PreToolUse)
            .iter()
            .map(|definition| match definition {
                HookDefinition::Command(hook) => hook.command.clone(),
                HookDefinition::Prompt(hook) => hook.prompt.clone(),
            })
            .collect();
        assert_eq!(ordered, vec!["echo b", "echo a", "echo c"]);
    }

    #[test]
    fn aggregated_result_blocked_and_reason() {
        let aggregated = AggregatedHookResult {
            results: vec![
                HookResult {
                    hook_type: "command".into(),
                    success: true,
                    ..Default::default()
                },
                HookResult {
                    hook_type: "command".into(),
                    success: false,
                    blocked: true,
                    output: "fallback output".into(),
                    ..Default::default()
                },
            ],
        };
        assert!(aggregated.blocked());
        // reason 为空回落 output
        assert_eq!(aggregated.reason(), "fallback output");
    }

    #[test]
    fn matcher_selects_payload_subject() {
        let hook = HookDefinition::Command(CommandHookDefinition {
            command: "true".into(),
            timeout_seconds: 5,
            matcher: Some("write_*".into()),
            block_on_failure: false,
            priority: 0,
        });
        let mut payload = Map::new();
        payload.insert("tool_name".into(), Value::String("write_file".into()));
        assert!(matches_hook(&hook, &payload));
        payload.insert("tool_name".into(), Value::String("read_file".into()));
        assert!(!matches_hook(&hook, &payload));
        // 无 tool_name 时回落 event 字段
        let mut payload = Map::new();
        payload.insert("event".into(), Value::String("write_event".into()));
        assert!(matches_hook(&hook, &payload));
    }

    #[test]
    fn parse_hook_json_lenient() {
        assert_eq!(parse_hook_json(r#"{"ok": true}"#), (true, String::new()));
        let (ok, reason) = parse_hook_json(r#"{"ok": false, "reason": "nope"}"#);
        assert!(!ok);
        assert_eq!(reason, "nope");
        assert!(parse_hook_json("  YES ").0);
        assert!(parse_hook_json("ok").0);
        let (ok, reason) = parse_hook_json("gibberish");
        assert!(!ok);
        assert_eq!(reason, "gibberish");
        let (ok, reason) = parse_hook_json("");
        assert!(!ok);
        assert_eq!(reason, "hook returned invalid JSON");
    }

    #[test]
    fn shell_quote_matches_shlex_semantics() {
        assert_eq!(shell_quote("abc-123"), "abc-123");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r#"'it'"'"'s'"#);
        assert_eq!(shell_quote(""), "''");
    }

    #[tokio::test]
    async fn command_hook_success_and_blocking_failure() {
        let mut registry = HookRegistry::new();
        registry.register(
            HookEvent::PreToolUse,
            command_hook("echo hook-ran", 0, true),
        );
        registry.register(HookEvent::PreToolUse, command_hook("exit 3", 0, true));
        let executor = command_executor(registry);
        let mut payload = Map::new();
        payload.insert("tool_name".into(), Value::String("write_file".into()));
        let aggregated = executor.execute(HookEvent::PreToolUse, &payload).await;
        assert_eq!(aggregated.results.len(), 2);
        assert!(aggregated.results[0].success);
        assert_eq!(aggregated.results[0].output, "hook-ran");
        // 成功时 reason 置空（review 修复回归）
        assert!(aggregated.results[0].reason.is_empty());
        assert!(!aggregated.results[1].success);
        assert!(aggregated.blocked());
        assert_eq!(
            aggregated.results[1].metadata.get("returncode"),
            Some(&serde_json::json!(3))
        );
    }

    #[tokio::test]
    async fn update_registry_hot_swaps_under_arc() {
        // 宿主持 Arc<HookExecutor> 时仍可热更新注册表（review 修复回归）
        let executor = std::sync::Arc::new(command_executor(HookRegistry::new()));
        let aggregated = executor.execute(HookEvent::Stop, &Map::new()).await;
        assert!(aggregated.results.is_empty());

        let mut replacement = HookRegistry::new();
        replacement.register(HookEvent::Stop, command_hook("exit 9", 0, true));
        executor.update_registry(replacement);
        let aggregated = executor.execute(HookEvent::Stop, &Map::new()).await;
        assert_eq!(aggregated.results.len(), 1);
        assert!(aggregated.blocked());
    }

    #[tokio::test]
    async fn command_hook_receives_event_env_and_arguments() {
        let mut registry = HookRegistry::new();
        registry.register(
            HookEvent::UserPromptSubmit,
            command_hook("printf '%s|%s' \"$AINS_HOOK_EVENT\" $ARGUMENTS", 0, false),
        );
        let executor = command_executor(registry);
        let mut payload = Map::new();
        payload.insert("prompt".into(), Value::String("hi".into()));
        let aggregated = executor
            .execute(HookEvent::UserPromptSubmit, &payload)
            .await;
        assert_eq!(aggregated.results.len(), 1);
        let output = &aggregated.results[0].output;
        assert!(output.starts_with("user_prompt_submit|"), "{output}");
        assert!(output.contains(r#"{"prompt":"hi"}"#), "{output}");
    }

    #[tokio::test]
    async fn command_hook_rejects_oversized_payload_before_sandbox_call() {
        let mut registry = HookRegistry::new();
        registry.register(HookEvent::Stop, command_hook("echo must-not-run", 0, true));
        let executor = command_executor(registry);
        let mut payload = Map::new();
        payload.insert(
            "output".into(),
            Value::String("x".repeat(HOOK_PAYLOAD_MAX_BYTES)),
        );
        let aggregated = executor.execute(HookEvent::Stop, &payload).await;
        assert!(aggregated.blocked());
        assert!(aggregated.reason().contains("payload exceeded"));
    }

    #[tokio::test]
    async fn command_hook_timeout_blocks_when_configured() {
        let mut registry = HookRegistry::new();
        registry.register(
            HookEvent::Stop,
            HookDefinition::Command(CommandHookDefinition {
                command: "sleep 5".into(),
                timeout_seconds: 1,
                matcher: None,
                block_on_failure: true,
                priority: 0,
            }),
        );
        let executor = command_executor(registry);
        let aggregated = executor.execute(HookEvent::Stop, &Map::new()).await;
        assert!(aggregated.blocked());
        assert!(aggregated.reason().contains("timed out after 1s"));
    }

    #[tokio::test]
    async fn command_hook_output_is_bounded() {
        let mut registry = HookRegistry::new();
        registry.register(
            HookEvent::Stop,
            command_hook("yes x | head -c 300000", 0, false),
        );
        let executor = command_executor(registry);
        let aggregated = executor.execute(HookEvent::Stop, &Map::new()).await;
        let result = &aggregated.results[0];
        assert!(result.output.ends_with("...[output truncated]..."));
        assert!(
            result.output.len() <= COMMAND_HOOK_STREAM_MAX_BYTES + 32,
            "bounded hook output unexpectedly large: {}",
            result.output.len()
        );
    }

    #[tokio::test]
    async fn prompt_hook_without_model_fails() {
        let mut registry = HookRegistry::new();
        registry.register(
            HookEvent::PreToolUse,
            HookDefinition::Prompt(PromptHookDefinition {
                prompt: "validate $ARGUMENTS".into(),
                model: None,
                timeout_seconds: 5,
                matcher: None,
                block_on_failure: true,
                priority: 0,
            }),
        );
        let executor = HookExecutor::new(registry, std::env::temp_dir());
        let aggregated = executor.execute(HookEvent::PreToolUse, &Map::new()).await;
        assert!(aggregated.blocked());
        assert!(aggregated.reason().contains("require a model client"));
    }

    #[tokio::test]
    async fn prompt_hook_rejects_oversized_payload_before_model_call() {
        let mut registry = HookRegistry::new();
        registry.register(
            HookEvent::PreToolUse,
            HookDefinition::Prompt(PromptHookDefinition {
                prompt: "validate $ARGUMENTS".into(),
                model: None,
                timeout_seconds: 5,
                matcher: None,
                block_on_failure: true,
                priority: 0,
            }),
        );
        let model = Arc::new(ScriptedModelClient::new(vec![]));
        let executor = HookExecutor::new(registry, std::env::temp_dir())
            .with_model(Arc::clone(&model) as Arc<_>, None);
        let mut payload = Map::new();
        payload.insert(
            "tool_input".into(),
            Value::String("x".repeat(HOOK_PAYLOAD_MAX_BYTES)),
        );
        let aggregated = executor.execute(HookEvent::PreToolUse, &payload).await;
        assert!(aggregated.blocked());
        assert!(aggregated.reason().contains("payload exceeded"));
        assert!(model.recorded_requests().is_empty());
    }

    #[tokio::test]
    async fn prompt_hook_rejects_model_output_over_hard_limit() {
        let mut registry = HookRegistry::new();
        registry.register(
            HookEvent::Stop,
            HookDefinition::Prompt(PromptHookDefinition {
                prompt: "validate".into(),
                model: None,
                timeout_seconds: 5,
                matcher: None,
                block_on_failure: true,
                priority: 0,
            }),
        );
        let model = Arc::new(ScriptedModelClient::new(vec![vec![
            ModelStreamEvent::TextDelta {
                text: "x".repeat(PROMPT_HOOK_MESSAGE_MAX_BYTES + 1),
            },
            ModelStreamEvent::Complete {
                message: ConversationMessage::from_user_text("unused"),
                usage: UsageSnapshot::default(),
                stop_reason: None,
            },
        ]]));
        let executor =
            HookExecutor::new(registry, std::env::temp_dir()).with_model(model as Arc<_>, None);
        let aggregated = executor.execute(HookEvent::Stop, &Map::new()).await;
        assert!(aggregated.blocked());
        assert!(aggregated.reason().contains("response exceeded"));
    }

    #[tokio::test]
    async fn command_hook_fails_closed_without_sandbox() {
        let mut registry = HookRegistry::new();
        registry.register(HookEvent::Stop, command_hook("echo must-not-run", 0, true));
        let executor = HookExecutor::new(registry, std::env::temp_dir());
        let aggregated = executor.execute(HookEvent::Stop, &Map::new()).await;
        assert!(aggregated.blocked());
        assert!(aggregated.reason().contains("requires shell capability"));
    }

    #[test]
    fn hook_definition_serde_discriminant() {
        let json = r#"{"type":"command","command":"echo hi"}"#;
        let parsed: HookDefinition = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, HookDefinition::Command(_)));
        let json = r#"{"type":"prompt","prompt":"check"}"#;
        let parsed: HookDefinition = serde_json::from_str(json).unwrap();
        match parsed {
            HookDefinition::Prompt(hook) => {
                // prompt hook 默认 block_on_failure = true（对齐基线）
                assert!(hook.block_on_failure);
                assert_eq!(hook.timeout_seconds, 30);
            }
            other => panic!("expected prompt hook, got {other:?}"),
        }
    }
}
