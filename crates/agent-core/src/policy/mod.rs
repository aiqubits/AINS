//! Policy 层：三态权限引擎 + Sandbox（对齐 OpenHarness `permissions/` + `sandbox/`）。

pub mod permission_engine;
pub mod sandbox;

pub use permission_engine::{
    PathRule, PermissionDecision, PermissionEngine, PermissionMode, PermissionPrompt,
    PermissionReply, PermissionRequest, PermissionSettings, SENSITIVE_PATH_PATTERNS,
};
pub use sandbox::{
    NoopSandbox, Sandbox, SandboxCapabilities, SandboxError, ShellOutcome, ShellRequest,
    validate_sandbox_path,
};
