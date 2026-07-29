//! Native 系统工具（对齐 OpenHarness `bash_tool.py`；Clipboard / Notification /
//! Screenshot 基线无对应物，按 AINS_PLAN 3.4 设计为宿主集成端口）。
//!
//! Shell Command 注册为普通 Tool，但执行路径**必经 Sandbox 层**：
//! NoopSandbox 占位守卫下返回"无操作权限"，本模块不直接调用
//! `std::process::Command`（平台沙箱随 Phase 7.1 落地）。

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::error::ToolError;
use crate::marker::MaybeSendSync;
use crate::policy::{Sandbox, SandboxError, ShellRequest};
use crate::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

pub const SHELL_DEFAULT_TIMEOUT_SECONDS: u64 = 600;
pub const SHELL_OUTPUT_CAP_CHARS: usize = 12_000;
/// 实际 Sandbox 管道捕获上限；完成后再按字符数整形为 12,000 字符。
pub const SHELL_CAPTURE_MAX_BYTES: usize = 256 * 1024;

/// shell 输出统一整形（对齐 bash_tool `_format_output`）：CRLF 归一、
/// 裁剪空白、空输出占位、超长截断标记。
pub fn format_shell_output(raw: &str) -> String {
    let text = raw.replace("\r\n", "\n");
    let text = text.trim();
    if text.is_empty() {
        return "(no output)".to_string();
    }
    let chars = text.chars().count();
    if chars > SHELL_OUTPUT_CAP_CHARS {
        let head: String = text.chars().take(SHELL_OUTPUT_CAP_CHARS).collect();
        format!("{head}\n...[truncated]...")
    } else {
        text.to_string()
    }
}

/// 执行 shell 命令（stdout/stderr 合并捕获），必经 Sandbox。
pub struct ShellCommandTool {
    sandbox: Arc<dyn Sandbox>,
}

impl ShellCommandTool {
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
    }
}

#[async_trait::async_trait]
impl Tool for ShellCommandTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "shell_command".into(),
            description: "Run a shell command in the local repository.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to execute"},
                    "cwd": {"type": "string", "description": "Working directory override"},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600,
                                        "default": SHELL_DEFAULT_TIMEOUT_SECONDS}
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidInput("missing required string field: command".into())
            })?;
        let cwd = match input.get("cwd").and_then(Value::as_str) {
            Some(dir) => {
                let path = std::path::PathBuf::from(dir);
                if path.is_absolute() {
                    path
                } else {
                    ctx.cwd.join(path)
                }
            }
            None => ctx.cwd.to_path_buf(),
        };
        let timeout_seconds = input
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(SHELL_DEFAULT_TIMEOUT_SECONDS)
            .clamp(1, 600);

        // 必须检查 shell 单项能力，而非沙箱任一能力是否可用；未来仅实现
        // filesystem/network policy 的后端不能因此被误认为可执行命令。
        if !self.sandbox.capabilities().shell {
            return Ok(ToolResult::err(format!(
                "无操作权限：sandbox '{}' does not provide shell execution",
                self.sandbox.name()
            )));
        }

        match self
            .sandbox
            .exec_shell(ShellRequest {
                command: command.to_string(),
                cwd,
                timeout: Duration::from_secs(timeout_seconds),
                max_output_bytes: SHELL_CAPTURE_MAX_BYTES,
            })
            .await
        {
            Ok(outcome) => Ok(shell_outcome_to_result(command, timeout_seconds, outcome)),
            Err(SandboxError::Unavailable(reason)) | Err(SandboxError::Execution(reason)) => {
                Ok(ToolResult::err(reason))
            }
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::System
    }
}

fn shell_outcome_to_result(
    command: &str,
    timeout_seconds: u64,
    outcome: crate::policy::ShellOutcome,
) -> ToolResult {
    let mut metadata = serde_json::Map::new();
    metadata.insert("returncode".into(), serde_json::json!(outcome.exit_code));
    if outcome.timed_out {
        metadata.insert("timed_out".into(), Value::Bool(true));
        let text = format_shell_output(&outcome.output);
        let mut parts = vec![format!(
            "Command timed out after {timeout_seconds} seconds."
        )];
        if text != "(no output)" {
            parts.push(String::new());
            parts.push("Partial output:".into());
            parts.push(text);
        }
        let _ = command;
        return ToolResult {
            output: parts.join("\n"),
            is_error: true,
            metadata: Value::Object(metadata),
        };
    }
    ToolResult {
        output: format_shell_output(&outcome.output),
        is_error: outcome.exit_code != Some(0),
        metadata: Value::Object(metadata),
    }
}

// ── 宿主系统集成端口（Clipboard / Notification / Screenshot）────────────

/// 桌面宿主能力端口：由 Dioxus 宿主在 Phase 6 注入实现；未注入时对应
/// 工具报"当前宿主不可用"（基线无对应物，AINS_PLAN 3.4 设计）。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait SystemIntegration: MaybeSendSync {
    async fn clipboard_read(&self) -> Result<String, String>;
    async fn clipboard_write(&self, text: &str) -> Result<(), String>;
    async fn notify(&self, title: &str, body: &str) -> Result<(), String>;
    /// 截屏并返回持久化后的图片路径。
    async fn screenshot(&self) -> Result<String, String>;
}

fn integration_unavailable(tool: &str) -> ToolResult {
    ToolResult::err(format!(
        "{tool} is unavailable: no system integration is attached to this host \
         (desktop host integration lands with Phase 6)"
    ))
}

/// 剪贴板读写。
pub struct ClipboardTool {
    integration: Option<Arc<dyn SystemIntegration>>,
}

impl ClipboardTool {
    pub fn new(integration: Option<Arc<dyn SystemIntegration>>) -> Self {
        Self { integration }
    }
}

#[async_trait::async_trait]
impl Tool for ClipboardTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "clipboard".into(),
            description: "Read or write the system clipboard.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["read", "write"]},
                    "text": {"type": "string", "description": "Text to write (for write)"}
                },
                "required": ["action"]
            }),
        }
    }

    fn is_read_only(&self, input: &Value) -> bool {
        // 剪贴板读取虽不修改系统状态，但会暴露隐私数据，不能进入权限引擎
        // 的普通只读自动放行分支。读写均要求用户确认或显式策略授权。
        let _ = input;
        false
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let Some(integration) = &self.integration else {
            return Ok(integration_unavailable("clipboard"));
        };
        let action = input.get("action").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidInput("missing required string field: action".into())
        })?;
        match action {
            "read" => match integration.clipboard_read().await {
                Ok(text) => Ok(ToolResult::ok(text)),
                Err(error) => Ok(ToolResult::err(error)),
            },
            "write" => {
                let text = input.get("text").and_then(Value::as_str).ok_or_else(|| {
                    ToolError::InvalidInput("missing required string field: text".into())
                })?;
                match integration.clipboard_write(text).await {
                    Ok(()) => Ok(ToolResult::ok("Clipboard updated")),
                    Err(error) => Ok(ToolResult::err(error)),
                }
            }
            other => Err(ToolError::InvalidInput(format!("unknown action: {other}"))),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::System
    }
}

/// 桌面通知。
pub struct NotificationTool {
    integration: Option<Arc<dyn SystemIntegration>>,
}

impl NotificationTool {
    pub fn new(integration: Option<Arc<dyn SystemIntegration>>) -> Self {
        Self { integration }
    }
}

#[async_trait::async_trait]
impl Tool for NotificationTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "notification".into(),
            description: "Show a desktop notification to the user.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "body": {"type": "string"}
                },
                "required": ["title", "body"]
            }),
        }
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let Some(integration) = &self.integration else {
            return Ok(integration_unavailable("notification"));
        };
        let title = input.get("title").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidInput("missing required string field: title".into())
        })?;
        let body = input
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("missing required string field: body".into()))?;
        match integration.notify(title, body).await {
            Ok(()) => Ok(ToolResult::ok("Notification sent")),
            Err(error) => Ok(ToolResult::err(error)),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::System
    }
}

/// 截屏。
pub struct ScreenshotTool {
    integration: Option<Arc<dyn SystemIntegration>>,
}

impl ScreenshotTool {
    pub fn new(integration: Option<Arc<dyn SystemIntegration>>) -> Self {
        Self { integration }
    }
}

#[async_trait::async_trait]
impl Tool for ScreenshotTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "screenshot".into(),
            description: "Capture a screenshot and return the saved image path.".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // 截屏属于隐私敏感读取，必须经用户确认或显式策略授权。
        false
    }

    async fn execute(
        &self,
        _input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let Some(integration) = &self.integration else {
            return Ok(integration_unavailable("screenshot"));
        };
        match integration.screenshot().await {
            Ok(path) => Ok(ToolResult::ok(format!("Screenshot saved to: {path}"))),
            Err(error) => Ok(ToolResult::err(error)),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::System
    }
}

/// 注册全部系统工具（宿主便捷入口）。
pub fn register_system_tools(
    runtime: &mut crate::tools::ToolRuntime,
    sandbox: Arc<dyn Sandbox>,
    integration: Option<Arc<dyn SystemIntegration>>,
) {
    runtime.register(Box::new(ShellCommandTool::new(sandbox)));
    runtime.register(Box::new(ClipboardTool::new(integration.clone())));
    runtime.register(Box::new(NotificationTool::new(integration.clone())));
    runtime.register(Box::new(ScreenshotTool::new(integration)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::NoopSandbox;
    use crate::tools::ToolMetadata;
    use std::path::Path;
    use std::sync::Mutex;

    #[tokio::test]
    async fn shell_via_noop_sandbox_returns_permission_denied_text() {
        let tool = ShellCommandTool::new(Arc::new(NoopSandbox));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = tool
            .execute(serde_json::json!({"command": "echo hi"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.output.contains("无操作权限"), "{}", result.output);
    }

    #[tokio::test]
    async fn relative_shell_cwd_is_anchored_to_context_cwd() {
        let sandbox = Arc::new(CwdRecordingSandbox(Mutex::new(None)));
        let tool = ShellCommandTool::new(Arc::clone(&sandbox) as Arc<_>);
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/work/project"),
            metadata: &mut metadata,
        };
        let result = tool
            .execute(
                serde_json::json!({"command": "true", "cwd": "subdir"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            *sandbox.0.lock().unwrap(),
            Some((
                std::path::PathBuf::from("/work/project/subdir"),
                SHELL_CAPTURE_MAX_BYTES,
            ))
        );
    }

    struct FakeSandbox {
        outcome: crate::policy::ShellOutcome,
    }

    struct NetworkOnlySandbox;

    struct CwdRecordingSandbox(Mutex<Option<(std::path::PathBuf, usize)>>);

    #[async_trait::async_trait]
    impl Sandbox for CwdRecordingSandbox {
        fn name(&self) -> &'static str {
            "cwd-recorder"
        }

        fn capabilities(&self) -> crate::policy::SandboxCapabilities {
            crate::policy::SandboxCapabilities {
                shell: true,
                ..Default::default()
            }
        }

        async fn exec_shell(
            &self,
            request: ShellRequest,
        ) -> Result<crate::policy::ShellOutcome, SandboxError> {
            *self.0.lock().unwrap() = Some((request.cwd, request.max_output_bytes));
            Ok(crate::policy::ShellOutcome {
                output: String::new(),
                exit_code: Some(0),
                timed_out: false,
            })
        }
    }

    #[async_trait::async_trait]
    impl Sandbox for NetworkOnlySandbox {
        fn name(&self) -> &'static str {
            "network-only"
        }

        fn capabilities(&self) -> crate::policy::SandboxCapabilities {
            crate::policy::SandboxCapabilities {
                shell: false,
                network_policy: true,
                filesystem_policy: false,
            }
        }

        async fn exec_shell(
            &self,
            _request: ShellRequest,
        ) -> Result<crate::policy::ShellOutcome, SandboxError> {
            panic!("exec_shell must not be called without the shell capability")
        }
    }

    #[async_trait::async_trait]
    impl Sandbox for FakeSandbox {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn capabilities(&self) -> crate::policy::SandboxCapabilities {
            crate::policy::SandboxCapabilities {
                shell: true,
                network_policy: false,
                filesystem_policy: false,
            }
        }

        async fn exec_shell(
            &self,
            _request: ShellRequest,
        ) -> Result<crate::policy::ShellOutcome, SandboxError> {
            Ok(self.outcome.clone())
        }
    }

    #[tokio::test]
    async fn shell_outcome_formatting_success_failure_timeout() {
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        // 成功
        let tool = ShellCommandTool::new(Arc::new(FakeSandbox {
            outcome: crate::policy::ShellOutcome {
                output: "done\r\n".into(),
                exit_code: Some(0),
                timed_out: false,
            },
        }));
        let result = tool
            .execute(serde_json::json!({"command": "x"}), &mut ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output, "done");
        assert_eq!(result.metadata["returncode"], serde_json::json!(0));
        // 非零退出码
        let tool = ShellCommandTool::new(Arc::new(FakeSandbox {
            outcome: crate::policy::ShellOutcome {
                output: "".into(),
                exit_code: Some(2),
                timed_out: false,
            },
        }));
        let result = tool
            .execute(serde_json::json!({"command": "x"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(result.output, "(no output)");
        // 超时
        let tool = ShellCommandTool::new(Arc::new(FakeSandbox {
            outcome: crate::policy::ShellOutcome {
                output: "partial".into(),
                exit_code: None,
                timed_out: true,
            },
        }));
        let result = tool
            .execute(
                serde_json::json!({"command": "x", "timeout_seconds": 7}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.output.contains("timed out after 7 seconds"));
        assert!(result.output.contains("Partial output:\npartial"));
        assert_eq!(result.metadata["timed_out"], Value::Bool(true));
    }

    #[tokio::test]
    async fn unrelated_sandbox_capability_does_not_enable_shell() {
        let tool = ShellCommandTool::new(Arc::new(NetworkOnlySandbox));
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = tool
            .execute(serde_json::json!({"command": "echo hi"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.output.contains("does not provide shell execution"));
    }

    #[test]
    fn shell_output_cap_and_normalization() {
        assert_eq!(format_shell_output("  \n"), "(no output)");
        assert_eq!(format_shell_output("a\r\nb"), "a\nb");
        let long = "x".repeat(SHELL_OUTPUT_CAP_CHARS + 5);
        let formatted = format_shell_output(&long);
        assert!(formatted.ends_with("\n...[truncated]..."));
        assert_eq!(
            formatted.chars().count(),
            SHELL_OUTPUT_CAP_CHARS + "\n...[truncated]...".chars().count()
        );
    }

    #[tokio::test]
    async fn host_integration_tools_report_unavailable_without_backend() {
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = ClipboardTool::new(None)
            .execute(serde_json::json!({"action": "read"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error && result.output.contains("no system integration"));
        let result = NotificationTool::new(None)
            .execute(serde_json::json!({"title": "t", "body": "b"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        let result = ScreenshotTool::new(None)
            .execute(Value::Null, &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        // 剪贴板读取为隐私敏感操作，不得走普通只读自动放行
        let clipboard = ClipboardTool::new(None);
        assert!(!clipboard.is_read_only(&serde_json::json!({"action": "read"})));
        assert!(!clipboard.is_read_only(&serde_json::json!({"action": "write"})));
        assert!(!ScreenshotTool::new(None).is_read_only(&Value::Null));
    }
}
