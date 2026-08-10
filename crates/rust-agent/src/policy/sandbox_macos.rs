//! macOS 平台 Sandbox（Phase 7.1 Layer 2）：`sandbox-exec`（Seatbelt）隔离。
//!
//! macOS 内核（XNU/Darwin）无 Linux 的 namespace/seccomp；其对等隔离是
//! Seatbelt——由 SBPL profile 描述的强制访问控制，经系统内建
//! `/usr/bin/sandbox-exec -p <profile> /bin/sh -c <cmd>` 施加（无需安装）。
//! 本适配以子进程包装方式复用它——无 unsafe。策略（[`SandboxPolicy`]）经
//! [`macos_sbpl_profile`] 推导为 SBPL：文件系统四象限 → `file-read*`/
//! `file-write*` 子树，网络 → `network*` 粗粒度开关。
//!
//! **未在真实 macOS 环境验证**：隔离代码已实现，但本机（Linux）无法运行
//! 验证。遵循默认拒绝原则，`capabilities().shell` 与 `exec_shell` 默认拒绝，
//! 仅当运维在 macOS 显式设置 `AINS_ENABLE_UNVERIFIED_SANDBOX=1` 后启用
//! （见 [`unverified_sandbox_enabled`]），以便在真实环境中验证。
//!
//! 平台特定依赖（`tokio::process`）仅在本 cfg 门控文件出现。

use std::ffi::OsString;
use std::path::PathBuf;

use crate::policy::sandbox::{
    Sandbox, SandboxCapabilities, SandboxError, ShellOutcome, ShellRequest, canonical_shell_cwd,
    unverified_sandbox_enabled,
};
use crate::policy::sandbox_policy::{SandboxPolicy, macos_sbpl_profile};

/// macOS `sandbox-exec`（Seatbelt）沙箱：策略在构造时固定，`exec_shell`
/// 据此生成 SBPL profile 并包装命令。
pub struct MacSandboxExecSandbox {
    policy: SandboxPolicy,
    /// 系统 `sandbox-exec` 的固定可信路径；缺失则拒绝执行。不得从 PATH
    /// 搜索，避免启用后被同名可执行文件替换成未施加 Seatbelt 的包装器。
    sandbox_exec: Option<PathBuf>,
}

impl MacSandboxExecSandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        let system_sandbox_exec = PathBuf::from("/usr/bin/sandbox-exec");
        let sandbox_exec = system_sandbox_exec.is_file().then_some(system_sandbox_exec);
        Self {
            policy,
            sandbox_exec,
        }
    }

    /// 测试注入：显式指定 sandbox-exec 路径（不触发 PATH 探测）。
    #[cfg(test)]
    fn with_path(policy: SandboxPolicy, sandbox_exec: Option<PathBuf>) -> Self {
        Self {
            policy,
            sandbox_exec,
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for MacSandboxExecSandbox {
    fn name(&self) -> &'static str {
        "macos-sandbox-exec"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        // 未 opt-in（默认）或 sandbox-exec 缺失 → 无能力（fail-closed 拒绝）。
        // 隔离已实现但未经真实环境验证，故默认关闭。
        if self.sandbox_exec.is_some()
            && unverified_sandbox_enabled()
            && self.policy.macos_shell_policy_is_enforceable()
        {
            SandboxCapabilities {
                shell: true,
                network_policy: true,
                filesystem_policy: true,
            }
        } else {
            SandboxCapabilities::default()
        }
    }

    async fn exec_shell(&self, mut request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
        // B1 纵深防御：cwd 为文件系统根时拒绝（SBPL 写子树恒含 cwd，根目录
        // 即全盘写）。工具层已约束 cwd ⊆ 工作区；本检查保护绕过工具层的
        // 直接调用方。`parent() == None` 即根目录（Unix "/"）。
        if request.cwd.parent().is_none() {
            return Err(SandboxError::Unavailable(
                "refusing to run shell with cwd at filesystem root: the SBPL write subtree \
                 would cover the entire filesystem"
                    .to_string(),
            ));
        }
        if !unverified_sandbox_enabled() {
            return Err(SandboxError::Unavailable(
                "无操作权限：macOS sandbox-exec 隔离已实现但未在真实环境验证，默认拒绝执行 \
                 shell（在 macOS 上设置 AINS_ENABLE_UNVERIFIED_SANDBOX=1 显式启用以验证；\
                 遵循默认拒绝原则不降级直跑）"
                    .to_string(),
            ));
        }
        if !self.policy.macos_shell_policy_is_enforceable() {
            return Err(SandboxError::Unavailable(
                "macOS sandbox-exec cannot faithfully enforce this shell network or filesystem deny policy; refusing execution"
                    .to_string(),
            ));
        }
        let Some(sandbox_exec) = self.sandbox_exec.clone() else {
            return Err(SandboxError::Unavailable(
                "无操作权限：未检测到 /usr/bin/sandbox-exec，拒绝在无隔离下执行 shell".to_string(),
            ));
        };
        request.cwd = canonical_shell_cwd(&request.cwd)?;
        if !self.policy.shell_cwd_is_allowed(&request.cwd) {
            return Err(SandboxError::Unavailable(
                "shell cwd is blocked by the sandbox filesystem policy; refusing to grant its implicit read/write subtree"
                    .to_string(),
            ));
        }
        run_sandbox_exec(&sandbox_exec, &self.policy, request).await
    }
}

/// 构造 sandbox-exec 参数（纯函数，供单测固化）：`-p <profile> /bin/sh -c <cmd>`。
fn build_sandbox_exec_args(profile: &str, command: &str) -> Vec<OsString> {
    vec![
        OsString::from("-p"),
        OsString::from(profile),
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from(command),
    ]
}

async fn run_sandbox_exec(
    sandbox_exec: &std::path::Path,
    policy: &SandboxPolicy,
    request: ShellRequest,
) -> Result<ShellOutcome, SandboxError> {
    use std::process::Stdio;

    let profile = macos_sbpl_profile(policy, &request.cwd);
    let args = build_sandbox_exec_args(&profile, &request.command);
    let mut command = tokio::process::Command::new(sandbox_exec);
    command
        .args(&args)
        .current_dir(&request.cwd)
        // Do not expose desktop credentials (for example AINS_API_TOKEN) to
        // the command.  The sandbox receives only non-sensitive defaults.
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        // Keep shell-created config and temporary files inside the already
        // policy-authorized working tree.  Granting /private/tmp here would
        // silently widen a restrictive allow_write policy.
        .env("HOME", &request.cwd)
        .env("TMPDIR", &request.cwd)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // 独立进程组：超时/取消时 killpg 可连 sh 及其孙进程一并终止
    // （kill_on_drop 只杀 sandbox-exec 本身，Seatbelt 包装的 shell 会残留）。
    command.process_group(0);

    let cap = request.max_output_bytes;
    let timeout = request.timeout;
    let cancel = request.cancel.clone();
    let stdout_sink = request.output_sink.clone();
    let stderr_sink = request.output_sink.clone();
    let mut child = command
        .spawn()
        .map_err(|error| SandboxError::Execution(format!("spawn sandbox-exec failed: {error}")))?;
    let child_pid = child.id().unwrap_or(0);
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();

    // 读管道时即强制字节上限（不先无界收集）；超限剩余由内核管道背压 +
    // 超时/取消分支 killpg 兜底（阻塞进程树被整组终止后 wait 收割）。
    // 超时/取消 future 在阶段间复用：**管道读完不等于进程退出**——输出超限后
    // 命令可能写端阻塞继续运行，wait 阶段仍需同一超时/取消兑底（回归修复：
    // 历史实现把 timeout 限制在读取阶段，超限命令会使 wait 无限挂起）。
    let timeout_fut = tokio::time::sleep(timeout);
    let cancel_fut = crate::policy::sandbox::wait_cancel(cancel);
    tokio::pin!(timeout_fut, cancel_fut);

    // 阶段 1：有界读管道（共享预算：合并输出 ≤ cap），与超时/取消竞争。
    // 预算耗尽后写端阻塞的剩余输出由内核管道背压 + 超时/取消分支 killpg
    // 兜底（阻塞进程树被整组终止后 wait 收割）。
    let budget = std::sync::atomic::AtomicUsize::new(cap);
    // Some(cancelled)：阶段 1 提前终止的原因（true = 取消标志，false = 超时）。
    let read_abort: Option<bool> = {
        let reading = async {
            tokio::join!(
                async {
                    if let Some(pipe) = stdout.as_mut() {
                        crate::policy::sandbox::read_bounded(
                            pipe,
                            &budget,
                            &mut out_buf,
                            stdout_sink.as_ref(),
                        )
                        .await;
                    }
                },
                async {
                    if let Some(pipe) = stderr.as_mut() {
                        crate::policy::sandbox::read_bounded(
                            pipe,
                            &budget,
                            &mut err_buf,
                            stderr_sink.as_ref(),
                        )
                        .await;
                    }
                },
            );
        };
        tokio::pin!(reading);
        tokio::select! {
            _ = &mut reading => None,
            _ = &mut timeout_fut => Some(false),
            _ = &mut cancel_fut => Some(true),
        }
    };

    if let Some(cancelled) = read_abort {
        // 终止整个进程组（sandbox-exec + sh + 孙进程），再 wait 收割避免僵尸；
        // 超时与取消共享同一终止路径（见 ShellRequest.cancel 契约）。
        if child_pid != 0 {
            crate::policy::sandbox::kill_process_group(child_pid);
        }
        let _ = child.wait().await;
        return Ok(ShellOutcome {
            output: crate::policy::sandbox::merge_shell_output(&out_buf, &err_buf),
            exit_code: None,
            timed_out: true,
            cancelled,
        });
    }

    // 阶段 2：等待退出码（输出超限后命令可能仍在运行；超时/取消继续兑底）。
    let status = tokio::select! {
        status = child.wait() => status,
        _ = &mut timeout_fut => {
            if child_pid != 0 {
                crate::policy::sandbox::kill_process_group(child_pid);
            }
            let _ = child.wait().await;
            return Ok(ShellOutcome {
                output: crate::policy::sandbox::merge_shell_output(&out_buf, &err_buf),
                exit_code: None,
                timed_out: true,
                cancelled: false,
            });
        }
        _ = &mut cancel_fut => {
            if child_pid != 0 {
                crate::policy::sandbox::kill_process_group(child_pid);
            }
            let _ = child.wait().await;
            return Ok(ShellOutcome {
                output: crate::policy::sandbox::merge_shell_output(&out_buf, &err_buf),
                exit_code: None,
                timed_out: true,
                cancelled: true,
            });
        }
    };
    let status = status
        .map_err(|error| SandboxError::Execution(format!("wait sandbox-exec failed: {error}")))?;
    Ok(ShellOutcome {
        output: crate::policy::sandbox::merge_shell_output(&out_buf, &err_buf),
        exit_code: status.code(),
        timed_out: false,
        cancelled: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn request(command: &str) -> ShellRequest {
        ShellRequest {
            command: command.to_string(),
            cwd: PathBuf::from("/work/project"),
            timeout: Duration::from_secs(5),
            max_output_bytes: 4096,
            cancel: None,
            output_sink: None,
        }
    }

    #[test]
    fn sandbox_exec_args_wrap_profile_and_shell() {
        let args: Vec<String> = build_sandbox_exec_args("(version 1)(deny default)", "echo hi")
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-p",
                "(version 1)(deny default)",
                "/bin/sh",
                "-c",
                "echo hi"
            ]
        );
    }

    #[tokio::test]
    async fn default_without_opt_in_reports_shell_unavailable_and_refuses() {
        // 默认（未设 AINS_ENABLE_UNVERIFIED_SANDBOX）：即便 sandbox-exec 存在，
        // 未验证隔离仍 fail-closed 拒绝，capabilities().shell=false。
        // 注：本测试假设 CI 环境未设置该 env var（默认拒绝语义）。
        let sandbox = MacSandboxExecSandbox::with_path(
            SandboxPolicy::default(),
            Some(PathBuf::from("/usr/bin/sandbox-exec")),
        );
        if !unverified_sandbox_enabled() {
            assert!(!sandbox.capabilities().shell);
            assert!(matches!(
                sandbox.exec_shell(request("echo hi")).await,
                Err(SandboxError::Unavailable(_))
            ));
        }
    }
}
