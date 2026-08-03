//! Policy 层：三态权限引擎 + Sandbox（对齐 OpenHarness `permissions/` + `sandbox/`）。

pub mod permission_engine;
pub mod sandbox;
pub mod sandbox_policy;

// 平台级 Sandbox 运行时（Phase 7.1 Layer 2）：仅在对应 target 编译。
// 平台特定依赖（tokio::process / which）只在这些 cfg 门控适配文件中出现。
#[cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
pub mod sandbox_linux;
#[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
pub mod sandbox_macos;
#[cfg(all(
    not(target_arch = "wasm32"),
    any(target_os = "android", target_os = "ios")
))]
pub mod sandbox_mobile;
#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
pub mod sandbox_windows;

pub use permission_engine::{
    PathRule, PermissionDecision, PermissionEngine, PermissionMode, PermissionPrompt,
    PermissionReply, PermissionRequest, PermissionSettings, SENSITIVE_PATH_PATTERNS,
};
pub use sandbox::{
    NoopSandbox, Sandbox, SandboxCapabilities, SandboxError, ShellOutcome, ShellOutputSink,
    ShellRequest, default_sandbox, resolve_shell_cwd_within_workspace, validate_sandbox_path,
};
pub use sandbox_policy::{DomainRule, FilesystemPolicy, NetworkPolicy, SandboxPolicy};
