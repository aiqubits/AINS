//! Sandbox trait 与 NoopSandbox 占位实现（对齐 OpenHarness `sandbox/`）。
//!
//! Phase 3 仅落地能力探测 + 路径边界校验 + 占位桩；平台级运行时
//! （Linux namespace/seccomp、macOS sandbox-exec、Windows Job Object）
//! 于 Phase 7.1 替换本占位。Shell 等高风险操作的执行路径**必经**本层：
//! `is_available() == false` 时拒绝执行，工具不得绕过直接调
//! `std::process::Command`。

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

use crate::marker::MaybeSendSync;

/// Sandbox 能力探测结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SandboxCapabilities {
    /// 是否可执行 shell 命令。
    pub shell: bool,
    /// 是否支持网络域白/黑名单策略。
    pub network_policy: bool,
    /// 是否支持文件系统读写四象限策略。
    pub filesystem_policy: bool,
}

#[derive(Debug, Error)]
pub enum SandboxError {
    /// 沙箱不可用（对齐基线 `SandboxUnavailableError`）：调用方将其归一化
    /// 为 is_error 的 tool_result，不中止会话。
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    #[error("sandbox execution failed: {0}")]
    Execution(String),
}

/// Shell 执行请求。
#[derive(Debug, Clone)]
pub struct ShellRequest {
    pub command: String,
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// stdout + stderr 合并捕获的字节上限。真实 Sandbox 后端必须在
    /// 读管道时强制此上限，不得先无界收集后再截断。
    pub max_output_bytes: usize,
}

/// Shell 执行结果（stdout/stderr 合并，对齐 bash_tool 输出口径）。
#[derive(Debug, Clone)]
pub struct ShellOutcome {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// 沙箱抽象：所有高风险系统操作的强制关口。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait Sandbox: MaybeSendSync {
    fn name(&self) -> &'static str;

    /// 能力探测：宿主平台是否具备隔离执行环境。
    fn capabilities(&self) -> SandboxCapabilities;

    /// 沙箱整体是否可用；`false` 时高风险操作一律拒绝。
    fn is_available(&self) -> bool {
        self.capabilities() != SandboxCapabilities::default()
    }

    /// 在沙箱内执行 shell 命令。后端必须同时强制
    /// `request.timeout` 与 `request.max_output_bytes`；占位实现必须返回
    /// `Unavailable`。
    async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError>;
}

/// 占位沙箱：能力全无、拒绝一切高风险操作（Phase 7.1 替换为平台级实现）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSandbox;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Sandbox for NoopSandbox {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::default()
    }

    async fn exec_shell(&self, _request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
        Err(SandboxError::Unavailable(
            "无操作权限：当前构建未启用平台沙箱运行时，shell 命令被拒绝执行 \
             （NoopSandbox；平台级沙箱随 Phase 7.1 落地）"
                .to_string(),
        ))
    }
}

/// 路径边界校验（对齐 `sandbox/path_validator.py`）：`path` 必须落在
/// `cwd` 或 `extra_allowed` 目录之内。返回 `Ok(())` 或拒绝原因。
///
/// 与基线差异（有意）：基线 `Path.resolve()` 对不存在的路径做纯词法解析，
/// Rust `canonicalize` 要求路径存在；此处统一采用**词法规范化**
/// （消解 `.` / `..`，不追符号链接），语义上是更保守的边界检查——
/// symlink 逃逸由 Phase 7.1 平台沙箱在 OS 层拦截。
pub fn validate_sandbox_path(
    path: &Path,
    cwd: &Path,
    extra_allowed: &[PathBuf],
) -> Result<(), String> {
    let resolved_cwd = lexical_normalize(cwd, None);
    let resolved = lexical_normalize(path, Some(&resolved_cwd));

    if resolved.starts_with(&resolved_cwd) {
        return Ok(());
    }
    for allowed in extra_allowed {
        let allowed = lexical_normalize(allowed, Some(&resolved_cwd));
        if resolved.starts_with(&allowed) {
            return Ok(());
        }
    }
    Err(format!(
        "path {} is outside the sandbox boundary ({})",
        resolved.display(),
        resolved_cwd.display()
    ))
}

/// 词法规范化：相对路径以 `base` 为锚，逐组件消解 `.` 与 `..`。
/// `..` 越过根时钳位在根（与 Python `resolve(strict=False)` 一致）。
fn lexical_normalize(path: &Path, base: Option<&Path>) -> PathBuf {
    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(base) = base {
        base.join(path)
    } else {
        path.to_path_buf()
    };
    let mut normalized = PathBuf::new();
    for component in anchored.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 保留根前缀，仅弹出普通组件
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn noop_sandbox_probes_unavailable() {
        let sandbox = NoopSandbox;
        assert!(!sandbox.is_available());
        assert_eq!(sandbox.capabilities(), SandboxCapabilities::default());
    }

    #[test]
    fn noop_sandbox_rejects_shell() {
        let sandbox = NoopSandbox;
        let result = futures::executor::block_on(sandbox.exec_shell(ShellRequest {
            command: "echo hi".into(),
            cwd: PathBuf::from("/tmp"),
            timeout: Duration::from_secs(1),
            max_output_bytes: 1024,
        }));
        match result {
            Err(SandboxError::Unavailable(reason)) => {
                assert!(reason.contains("无操作权限"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn path_inside_cwd_is_allowed() {
        let cwd = Path::new("/work/project");
        assert!(validate_sandbox_path(Path::new("src/main.rs"), cwd, &[]).is_ok());
        assert!(validate_sandbox_path(Path::new("/work/project/a/b"), cwd, &[]).is_ok());
    }

    #[test]
    fn escape_via_parent_components_is_rejected() {
        let cwd = Path::new("/work/project");
        let err = validate_sandbox_path(Path::new("../secrets.txt"), cwd, &[]).unwrap_err();
        assert!(err.contains("outside the sandbox boundary"), "{err}");
        // 深层 .. 逃逸同样拦截
        assert!(validate_sandbox_path(Path::new("a/../../b"), cwd, &[]).is_err());
        // .. 消解后仍在边界内则放行
        assert!(validate_sandbox_path(Path::new("a/../src/lib.rs"), cwd, &[]).is_ok());
    }

    #[test]
    fn extra_allowed_directories_pass() {
        let cwd = Path::new("/work/project");
        let extra = vec![PathBuf::from("/data/shared")];
        assert!(validate_sandbox_path(Path::new("/data/shared/x.csv"), cwd, &extra).is_ok());
        assert!(validate_sandbox_path(Path::new("/data/other/x.csv"), cwd, &extra).is_err());
    }

    #[test]
    fn parent_traversal_clamps_at_root() {
        let cwd = Path::new("/w");
        // /../../etc/passwd → /etc/passwd，仍在边界外
        assert!(validate_sandbox_path(Path::new("/../../etc/passwd"), cwd, &[]).is_err());
    }
}
