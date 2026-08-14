//! Sandbox trait 与 NoopSandbox 占位实现（对齐 Harness `sandbox/`）。
//!
//! Phase 3 仅落地能力探测 + 路径边界校验 + 占位桩；平台级运行时
//! （Linux namespace；seccomp profile 待接线；macOS sandbox-exec、Windows Job Object）
//! 于 Phase 7.1 替换本占位。Shell 等高风险操作的执行路径**必经**本层：
//! `is_available() == false` 时拒绝执行，工具不得绕过直接调
//! `std::process::Command`。

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
// AtomicUsize / Ordering 仅供 read_bounded / wait_cancel（Native-only）使用，
// wasm 下避免未使用告警。
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use thiserror::Error;

use crate::marker::MaybeSendSync;
use crate::policy::sandbox_policy::SandboxPolicy;

/// 未验证平台沙箱（macOS sandbox-exec / Windows Job Object）的显式启用开关。
///
/// macOS/Windows 隔离代码已实现但未在真实环境验证，遵循默认拒绝原则：
/// 默认关闭（`capabilities().shell=false`，shell fail-closed），仅当运维在对应
/// 平台显式设置 `AINS_ENABLE_UNVERIFIED_SANDBOX=1`（或 `true`）后启用，以便在
/// 真实环境中验证。Linux(bwrap) 已验证，不受此开关约束。
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn unverified_sandbox_enabled() -> bool {
    matches!(
        std::env::var("AINS_ENABLE_UNVERIFIED_SANDBOX").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

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

/// Optional, synchronous sink for bounded shell output chunks.
///
/// Native backends invoke it only for bytes that have already reserved space
/// from [`ShellRequest::max_output_bytes`].  It lets long-running callers
/// observe output without weakening the backend's global capture limit.
type OutputCallback = dyn Fn(&[u8]) + Send + Sync;

#[derive(Clone)]
pub struct ShellOutputSink(Arc<OutputCallback>);

impl std::fmt::Debug for ShellOutputSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ShellOutputSink(..)")
    }
}

impl ShellOutputSink {
    pub fn new(callback: impl Fn(&[u8]) + Send + Sync + 'static) -> Self {
        Self(Arc::new(callback))
    }

    pub fn push(&self, bytes: &[u8]) {
        (self.0)(bytes);
    }
}

/// Shell 执行请求。
#[derive(Debug, Clone)]
pub struct ShellRequest {
    pub command: String,
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// stdout + stderr 合并捕获的字节上限。真实 Sandbox 后端必须在
    /// 读管道时强制此上限（共享预算，合并输出不得超过），不得先无界
    /// 收集后再截断。
    pub max_output_bytes: usize,
    /// 协作式取消标志（可选）：置位后后端**必须**终止整个进程树并尽快返回。
    /// 与 `timeout` 等效的终止语义——用于宿主侧的显式取消（如后台任务
    /// stop），后端实现应把取消与超时视为同一类终止路径（kill 进程树）。
    /// 返回的 `ShellOutcome.timed_out = true`，且 `ShellOutcome.cancelled`
    /// 区分终止原因（`true` = 取消标志触发，`false` = 超时触发）。
    pub cancel: Option<Arc<AtomicBool>>,
    /// 可选的增量输出接收器。普通前台 shell 不设置；后台任务使用它把已
    /// 有界的输出即时写入任务记录，供轮询读取。
    pub output_sink: Option<ShellOutputSink>,
}

/// Resolve the working directory a native shell backend will actually enter.
/// A lexical `cwd` inside the workspace can still be a symlink to a broader
/// host tree; binding/chdir-ing that lexical path would then defeat both the
/// root guard and filesystem allowlists.  Shell working directories must
/// exist, so failure to canonicalize is a safe fail-closed error.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn canonical_shell_cwd(cwd: &Path) -> Result<PathBuf, SandboxError> {
    let resolved = std::fs::canonicalize(cwd).map_err(|error| {
        SandboxError::Unavailable(format!(
            "cannot resolve shell cwd {} for sandbox enforcement: {error}",
            cwd.display()
        ))
    })?;
    if resolved.parent().is_none() {
        return Err(SandboxError::Unavailable(
            "refusing to run shell with cwd at filesystem root: the resolved working tree \
             would expose the entire host filesystem"
                .to_string(),
        ));
    }
    Ok(resolved)
}

/// Resolve a shell working directory and require it to remain inside the
/// resolved workspace. Unlike [`validate_sandbox_path`], this deliberately
/// follows symlinks: a shell backend grants its cwd a writable bind/subtree,
/// so lexical containment alone would allow a workspace symlink to expose an
/// unrelated host directory. Both paths must already exist because a shell
/// cannot safely enter a non-existent working directory.
pub fn resolve_shell_cwd_within_workspace(cwd: &Path, workspace: &Path) -> Result<PathBuf, String> {
    if !workspace.is_absolute() {
        return Err(format!(
            "workspace {} must be absolute for shell boundary enforcement",
            workspace.display()
        ));
    }
    let workspace = std::fs::canonicalize(workspace).map_err(|error| {
        format!(
            "cannot resolve workspace {} for shell boundary enforcement: {error}",
            workspace.display()
        )
    })?;
    let cwd = std::fs::canonicalize(cwd).map_err(|error| {
        format!(
            "cannot resolve shell cwd {} for workspace boundary enforcement: {error}",
            cwd.display()
        )
    })?;
    if !workspace.is_dir() || !cwd.is_dir() {
        return Err("workspace and shell cwd must both be directories".to_string());
    }
    if cwd.starts_with(&workspace) {
        Ok(cwd)
    } else {
        Err(format!(
            "resolved shell cwd {} is outside the resolved workspace {}",
            cwd.display(),
            workspace.display()
        ))
    }
}

/// 终止指定进程所在的整个进程组（`kill(-pgid, SIGKILL)`，Unix）。
///
/// 与 [`tokio::process::Command::process_group`] 配套：后端在 spawn 时以
/// `process_group(0)` 把子进程设为新进程组组长（pgid = 子进程 pid），超时 /
/// 取消时对本组 SIGKILL，保证 shell 派生的孙进程一并终止（`kill_on_drop`
/// 仅杀直接子进程，`sh -c` 的后代会残留）。
///
/// **防御性守卫**（review 修复）：`kill(2)` 对负 pid 的特殊语义要求 pgid
/// 必须有效——`pid=0` 表示向**调用者所在进程组**发信号（会误杀 agent 自身
/// 进程组），`-(i32::MIN)` 负号运算会溢出（debug panic）；真实 pid 恒在
/// 有效范围内，此处对无效输入 no-op。
#[cfg(unix)]
pub(crate) fn kill_process_group(child_pid: u32) {
    // 0（id() 在极端竞态下可能返回 None 的兜底）与超出 i32 范围的输入
    // 均不得进入 kill(2)；否则 kill(0) 误杀自身进程组 / 溢出 panic。
    if child_pid == 0 || child_pid > i32::MAX as u32 {
        return;
    }
    // SAFETY: kill(2) 是 async-signal-safe 的 libc 函数；负 pid 表示进程组。
    // 进程组不存在（ESRCH）时忽略——子进程可能在竞态窗口已退出。
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    unsafe {
        kill(-(child_pid as i32), SIGKILL);
    }
}

/// 等待取消标志置位（供 select 分支与超时竞争）。
/// 仅 Native（Linux/macOS/Windows 沙箱后端与 tasks 使用；wasm 无 tokio
/// 定时器，且 wasm 沙箱为 Noop/Mobile——不消费本函数）。
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn wait_cancel(flag: Option<Arc<AtomicBool>>) {
    if let Some(flag) = flag {
        // 轮询式等待：取消是罕见路径，开销可忽略；轮询保证双 target 无
        // 额外依赖（无 tokio watch / notify）。
        while !flag.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    } else {
        // 无取消标志：永远不触发（等待方依赖 timeout 分支兜底）。
        std::future::pending::<()>().await;
    }
}

/// 有界读管道：与另一条流**共享** `budget`，保证两条流的合并输出不超过
/// 预算（`ShellRequest.max_output_bytes` 的精确语义；历史实现为每条流
/// 各自 `take(cap)`，合并可达 2×cap）。
///
/// 每轮读取前经 CAS 预留 `want` 字节，避免并发双流超额；实际读到
/// `n < want`（含 EOF）时归还未用预算。预算耗尽即停止读取——写端阻塞
/// 的剩余输出由超时/取消分支 killpg / kill-on-close 兜底终止。
/// 仅 Native（Linux/macOS/Windows 沙箱后端使用）。
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn read_bounded<R: tokio::io::AsyncRead + Unpin + ?Sized>(
    pipe: &mut R,
    budget: &AtomicUsize,
    out: &mut Vec<u8>,
    output_sink: Option<&ShellOutputSink>,
) {
    use tokio::io::AsyncReadExt;
    const CHUNK: usize = 8192;
    let mut buf = [0u8; CHUNK];
    loop {
        let remaining = budget.load(Ordering::Relaxed);
        if remaining == 0 {
            break;
        }
        let want = remaining.min(CHUNK);
        if budget
            .compare_exchange_weak(
                remaining,
                remaining - want,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_err()
        {
            // 另一条流已抢先预留：重读剩余预算（含 0 → 停止）。
            continue;
        }
        let n = match pipe.read(&mut buf[..want]).await {
            Ok(n) => n,
            // 管道读错误：归还预留预算（避免本流错误饿死另一条流），然后截断。
            Err(_) => {
                budget.fetch_add(want, Ordering::Relaxed);
                break;
            }
        };
        if n == 0 {
            // EOF：归还未用预算（供另一条流继续消费），然后停止本流。
            budget.fetch_add(want, Ordering::Relaxed);
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if let Some(sink) = output_sink {
            sink.push(&buf[..n]);
        }
        if n < want {
            // 归还未用预算，供另一条流继续消费。
            budget.fetch_add(want - n, Ordering::Relaxed);
        }
    }
}

/// Shell 执行结果（stdout/stderr 合并，对齐 bash_tool 输出口径）。
#[derive(Debug, Clone)]
pub struct ShellOutcome {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// `timed_out == true` 时区分终止原因：`true` = 协作式取消标志
    /// （`ShellRequest.cancel`）触发，`false` = 超时触发。
    pub cancelled: bool,
}

/// Merge bounded stdout/stderr buffers without discarding partial output when
/// a command is terminated by timeout or cooperative cancellation.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn merge_shell_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut combined = Vec::with_capacity(stdout.len().saturating_add(stderr.len()));
    combined.extend_from_slice(stdout);
    combined.extend_from_slice(stderr);
    String::from_utf8_lossy(&combined).into_owned()
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

/// 按运行平台选择默认 Sandbox（仿 `default_runtime_adapter` 模式）：
/// - Desktop Linux → bubblewrap 真实进程隔离；
/// - Desktop macOS/Windows → sandbox-exec / Job Object（代码已实现，opt-in，未环境验证）；
/// - Mobile(Android/iOS) → [`MobileSandbox`]：OS 应用沙箱自带隔离，shell 恒不可用
///   （平台设计），文件/网络策略由 Layer 1 强制；
/// - Web(WASM) / 其它 → [`NoopSandbox`]：浏览器自带隔离，agent 不派生子进程。
///
/// `policy` 下推进平台运行时（bwrap `--ro-bind`/`--bind`/网络开关；SBPL）。
// cfg 分发模式：每个 target 仅一个臂 return，其余 cfg'd out；needless_return
// 在单一编译臂上会误报，故全局允许。
#[allow(clippy::needless_return)]
#[cfg_attr(
    not(all(not(target_arch = "wasm32"), target_os = "linux")),
    allow(unused_variables, clippy::arc_with_non_send_sync)
)]
pub fn default_sandbox(policy: SandboxPolicy) -> Arc<dyn Sandbox> {
    #[cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
    {
        return Arc::new(crate::policy::sandbox_linux::LinuxBubblewrapSandbox::new(
            policy,
        ));
    }
    #[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
    {
        return Arc::new(crate::policy::sandbox_macos::MacSandboxExecSandbox::new(
            policy,
        ));
    }
    #[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
    {
        return Arc::new(crate::policy::sandbox_windows::WindowsJobObjectSandbox::new(policy));
    }
    #[cfg(all(
        not(target_arch = "wasm32"),
        any(target_os = "android", target_os = "ios")
    ))]
    {
        return Arc::new(crate::policy::sandbox_mobile::MobileSandbox::new(policy));
    }
    #[cfg(not(all(
        not(target_arch = "wasm32"),
        any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows",
            target_os = "android",
            target_os = "ios"
        )
    )))]
    {
        Arc::new(NoopSandbox)
    }
}

/// 占位沙箱：能力全无、拒绝一切高风险操作（环境自带隔离时的默认，
/// 以及平台运行时不可用时的 fail-closed 兜底）。
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
/// （消解 `.` / `..`，不追符号链接）。需要授予可写工作目录的 shell
/// 工具必须改用 [`resolve_shell_cwd_within_workspace`] 做真实路径校验；
/// 文件工具则在实际操作前单独拒绝符号链接组件。
pub fn validate_sandbox_path(
    path: &Path,
    cwd: &Path,
    extra_allowed: &[PathBuf],
) -> Result<(), String> {
    // 防御：空 cwd 无法提供有意义的边界，fail-closed 拒绝。
    if cwd.as_os_str().is_empty() {
        return Err("sandbox cwd is empty, cannot validate path boundary".to_string());
    }
    let resolved_cwd = lexical_normalize(cwd, None);
    // review 修复：词法规范化后为空（如 "." / ".." 相对路径）的 cwd 同样
    // 无法提供边界——空前缀 `starts_with("")` 恒 true，绝对路径逃逸被放行
    // （fail-open）。必须拒绝（相对 cwd 的调用方应先用工作区锚定绝对化）。
    if resolved_cwd.as_os_str().is_empty() {
        return Err(
            "sandbox cwd resolves to empty (relative path); cannot validate path boundary"
                .to_string(),
        );
    }
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
        "path is outside the sandbox boundary (cwd: {})",
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
            cancel: None,
            output_sink: None,
        }));
        match result {
            Err(SandboxError::Unavailable(reason)) => {
                assert!(reason.contains("无操作权限"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn merge_shell_output_keeps_partial_stdout_and_stderr() {
        assert_eq!(merge_shell_output(b"out", b"err"), "outerr");
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

    #[test]
    fn empty_cwd_is_rejected() {
        // 空 cwd 无法提供有意义的边界，fail-closed 拒绝。
        let err = validate_sandbox_path(Path::new("anyfile"), Path::new(""), &[]).unwrap_err();
        assert!(err.contains("cwd is empty"), "{err}");
        // 即使 extra_allowed 提供目录，空 cwd 仍被拒绝。
        let err = validate_sandbox_path(
            Path::new("/data/file"),
            Path::new(""),
            &[PathBuf::from("/data")],
        )
        .unwrap_err();
        assert!(err.contains("cwd is empty"), "{err}");
    }

    #[test]
    fn relative_cwd_does_not_disable_boundary() {
        // review 修复回归：词法规范化后为空（如 "." 或 ".."）的相对 cwd 会
        // 使边界退化为空前缀（`starts_with("")` 恒 true）——绝对路径逃逸
        // （如 "/etc/passwd"）被放行，fail-open。必须 fail-closed 拒绝。
        for rel in [".", "..", "./"] {
            let err =
                validate_sandbox_path(Path::new("/etc/passwd"), Path::new(rel), &[]).unwrap_err();
            assert!(
                err.contains("cwd"),
                "relative cwd {rel:?} must be rejected fail-closed, got: {err}"
            );
            // 相对路径在相对 cwd 下同样无法提供边界（拒绝）。
            assert!(validate_sandbox_path(Path::new("secret.txt"), Path::new(rel), &[]).is_err());
        }
        // 绝对 cwd 不受影响：边界仍生效（工作区内放行、区外拒绝）。
        assert!(validate_sandbox_path(Path::new("/work/a.rs"), Path::new("/work"), &[]).is_ok());
        assert!(validate_sandbox_path(Path::new("/etc/x"), Path::new("/work"), &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn shell_cwd_rejects_workspace_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside = workspace.path().join("inside");
        std::fs::create_dir(&inside).unwrap();
        let escaped = workspace.path().join("escaped");
        symlink(outside.path(), &escaped).unwrap();

        assert_eq!(
            resolve_shell_cwd_within_workspace(&inside, workspace.path()).unwrap(),
            std::fs::canonicalize(&inside).unwrap()
        );
        let error = resolve_shell_cwd_within_workspace(&escaped, workspace.path()).unwrap_err();
        assert!(error.contains("outside the resolved workspace"), "{error}");
    }

    #[tokio::test]
    async fn read_bounded_shared_budget_limits_combined_output() {
        // review 修复回归：两条流共享预算，合并输出不得超过 cap
        // （历史实现每条流各自 take(cap)，合并可达 2×cap）。
        use std::sync::atomic::AtomicUsize;
        use tokio::io::AsyncWriteExt;

        let (mut tx1, mut rx1) = tokio::io::duplex(1024);
        let (mut tx2, mut rx2) = tokio::io::duplex(1024);
        // 每条流 1000 字节，预算 1200：合并必须截断在 1200 内。
        tx1.write_all(&vec![b'a'; 1000]).await.unwrap();
        tx2.write_all(&vec![b'b'; 1000]).await.unwrap();
        tx1.shutdown().await.unwrap();
        tx2.shutdown().await.unwrap();

        let budget = AtomicUsize::new(1200);
        let mut out1 = Vec::new();
        let mut out2 = Vec::new();
        tokio::join!(
            read_bounded(&mut rx1, &budget, &mut out1, None),
            read_bounded(&mut rx2, &budget, &mut out2, None),
        );
        assert!(
            out1.len() + out2.len() <= 1200,
            "combined output {} > shared cap 1200 (out1={}, out2={})",
            out1.len() + out2.len(),
            out1.len(),
            out2.len()
        );
        // 预算最终耗尽（无泄漏）。
        assert_eq!(budget.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn read_bounded_zero_budget_reads_nothing() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::AsyncWriteExt;

        let (mut tx, mut rx) = tokio::io::duplex(64);
        tx.write_all(b"data").await.unwrap();
        tx.shutdown().await.unwrap();
        let budget = AtomicUsize::new(0);
        let mut out = Vec::new();
        read_bounded(&mut rx, &budget, &mut out, None).await;
        assert!(out.is_empty(), "zero budget must not read any bytes");
    }

    #[tokio::test]
    async fn read_bounded_returns_unused_budget_on_short_read() {
        // EOF 短读：实际读到 n < want 时归还未用预算（供另一条流消费）。
        use std::sync::atomic::AtomicUsize;
        use tokio::io::AsyncWriteExt;

        let (mut tx, mut rx) = tokio::io::duplex(64);
        tx.write_all(b"abc").await.unwrap();
        tx.shutdown().await.unwrap();
        let budget = AtomicUsize::new(4096);
        let mut out = Vec::new();
        read_bounded(&mut rx, &budget, &mut out, None).await;
        assert_eq!(out, b"abc");
        assert_eq!(
            budget.load(Ordering::Relaxed),
            4096 - 3,
            "unused budget must be returned after EOF"
        );
    }
}
