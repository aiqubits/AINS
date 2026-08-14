//! Linux 平台 Sandbox（Phase 7.1 Layer 2）：bubblewrap（bwrap）进程隔离。
//!
//! bwrap 在内核层用 namespace 隔离子进程（Flatpak 同款、经审计），
//! 本适配以子进程包装方式复用它——无 unsafe。当前实现尚未安装自定义
//! seccomp-BPF profile；需要 syscall 级限制的部署必须继续保持 shell 关闭。
//! 策略（[`SandboxPolicy`]）
//! 下推为 bwrap 参数：文件系统四象限 → `--ro-bind`/`--bind`，网络 →
//! namespace 隔离 +（可选）`--share-net`。
//!
//! 降级语义（遵循 AINS 原则，与 Harness `wrap_command_for_sandbox` 相反）：
//! bwrap 不存在时 `capabilities().shell=false`，shell 被**拒绝**执行，
//! 绝不降级为宿主直跑。
//!
//! 平台特定依赖（`tokio::process`、`which`）仅在本 cfg 门控文件出现。

use std::ffi::OsString;
use std::path::PathBuf;

use crate::policy::sandbox::{
    Sandbox, SandboxCapabilities, SandboxError, ShellOutcome, ShellRequest, canonical_shell_cwd,
};
use crate::policy::sandbox_policy::SandboxPolicy;

/// 只读绑定的系统目录（存在才绑定，用 `--ro-bind-try` 容忍缺失）。
///
/// `/etc` 不在此列表：将整个目录作为运行时便利项会绕过 `allow_read`
/// 的工作区白名单。网络/证书所需的少量配置在下方以精确路径单独绑定。
const SYSTEM_RO_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64"];
/// Shell 运行时的最小 `/etc` 只读例外。它们用于名称解析和 TLS 验证，
/// 但不应把机器配置（例如 `/etc/hostname`）暴露给 agent。
const SYSTEM_RO_FILES: &[&str] = &["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"];
const SYSTEM_RO_CERT_DIR: &str = "/etc/ssl/certs";

/// bubblewrap 沙箱：策略在构造时固定，`exec_shell` 据此包装命令。
pub struct LinuxBubblewrapSandbox {
    policy: SandboxPolicy,
    /// 启动时探测到的 bwrap 可执行路径；`None` 表示不可用（拒绝执行）。
    bwrap: Option<PathBuf>,
}

impl LinuxBubblewrapSandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        let bwrap = which_bwrap();
        Self { policy, bwrap }
    }

    /// 测试注入：显式指定 bwrap 路径（不触发 PATH 探测）。
    #[cfg(test)]
    fn with_bwrap(policy: SandboxPolicy, bwrap: Option<PathBuf>) -> Self {
        Self { policy, bwrap }
    }
}

/// 在 PATH 中查找 bwrap 并做一次最小冒烟验证（`which` crate：校验可执行位）。
///
/// 冒烟覆盖"bwrap 存在但无法创建 namespace"的情形（如非 setuid 安装 + 系统
/// 禁用 unprivileged user namespaces）：此时 `capabilities().shell=true` 会误导
/// 调用方——命令将在运行时才失败而非明确拒绝，与 fail-closed 语义不符，
/// 故视为不可用。
fn which_bwrap() -> Option<PathBuf> {
    let path = which::which("bwrap").ok()?;
    if smoke_test_bwrap(&path) {
        Some(path)
    } else {
        None
    }
}

/// 冒烟探测：`bwrap --unshare-all --die-with-parent /bin/true` 需 0 退出。
/// 仅探测能力（创建 namespace 后立即退出），不执行任何业务命令。
fn smoke_test_bwrap(path: &std::path::Path) -> bool {
    std::process::Command::new(path)
        .args(["--unshare-all", "--die-with-parent", "/bin/true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[async_trait::async_trait]
impl Sandbox for LinuxBubblewrapSandbox {
    fn name(&self) -> &'static str {
        "linux-bubblewrap"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        // bwrap 缺失 → 无任何隔离能力（默认拒绝）；存在 → shell + 文件/网络策略。
        if self.bwrap.is_some() {
            SandboxCapabilities {
                shell: true,
                network_policy: self.policy.network.allowed_domains.is_empty()
                    && (self.policy.network.denied_domains.is_empty()
                        || self.policy.network.blocks_all()),
                filesystem_policy: self.policy.filesystem.deny_read.is_empty()
                    && self.policy.filesystem.deny_write.is_empty()
                    && !has_write_bind_outside_read_policy(&self.policy),
            }
        } else {
            SandboxCapabilities::default()
        }
    }

    async fn exec_shell(&self, mut request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
        // B1 纵深防御：cwd 为文件系统根时拒绝（`--bind / /` 可写绑定使整个
        // 文件系统在沙箱内可写，沙箱即宿主）。工具层已约束 cwd ⊆ 工作区；
        // 本检查保护绕过工具层的直接调用方（如未来新接入的后台执行路径）。
        // `parent() == None` 即根目录（Unix "/"；Windows "C:\"）。
        if request.cwd.parent().is_none() {
            return Err(SandboxError::Unavailable(
                "refusing to run shell with cwd at filesystem root: a writable root bind \
                 would expose the entire host filesystem"
                    .to_string(),
            ));
        }
        let Some(bwrap) = self.bwrap.clone() else {
            return Err(SandboxError::Unavailable(
                "无操作权限：未检测到 bubblewrap（bwrap）运行时，拒绝在无隔离下执行 shell \
                 （安装 bubblewrap 以启用沙箱化 shell；Phase 7.1 遵循默认拒绝原则不降级直跑）"
                    .to_string(),
            ));
        };
        request.cwd = canonical_shell_cwd(&request.cwd)?;
        // Re-check after canonicalization: a direct caller can supply a
        // lexical alias such as `/tmp/..`, which becomes `/` only after the
        // initial root guard above. Binding that resolved root read-write
        // would expose the whole host filesystem inside bubblewrap.
        if request.cwd.parent().is_none() {
            return Err(SandboxError::Unavailable(
                "refusing to run shell with cwd at filesystem root: a writable root bind \
                 would expose the entire host filesystem"
                    .to_string(),
            ));
        }
        // Check the resolved directory, not a lexical path that could pass
        // through a symlink into a broader host tree.
        if !self.policy.shell_cwd_is_allowed(&request.cwd) {
            return Err(SandboxError::Unavailable(
                "shell cwd is blocked by the sandbox filesystem policy; refusing to grant its implicit read/write bind"
                    .to_string(),
            ));
        }
        if (!self.policy.network.allowed_domains.is_empty()
            || (!self.policy.network.denied_domains.is_empty()
                && !self.policy.network.blocks_all()))
            || !self.policy.filesystem.deny_read.is_empty()
            || !self.policy.filesystem.deny_write.is_empty()
            || has_write_bind_outside_read_policy(&self.policy)
        {
            return Err(SandboxError::Unavailable(
                "bubblewrap cannot faithfully enforce this domain or filesystem policy for shell; refusing execution"
                    .to_string(),
            ));
        }
        run_bwrap(&bwrap, &self.policy, request).await
    }
}

/// 构造 bwrap 参数（纯函数，供单测固化）。返回 bwrap 之后的完整 argv，
/// 末尾为 `-- sh -c <command>`。
fn build_bwrap_args(policy: &SandboxPolicy, request: &ShellRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();

    // 全 namespace 隔离（net/pid/ipc/uts/cgroup/user），父进程死则子进程死。
    args.push(OsString::from("--unshare-all"));
    args.push(OsString::from("--die-with-parent"));
    // 独立会话，防止 TIOCSTI 向宿主终端注入。
    args.push(OsString::from("--new-session"));

    // 网络：粗粒度开关。策略未整体封锁（deny "*"）时共享网络，使 git/curl
    // 等可用；域名级白/黑名单由 Layer 1（web_fetch）精确执行——bwrap 无
    // 内建域名过滤，精细化 shell 出网控制需 netns+代理（Phase 7+）。
    if !policy.network.blocks_all() {
        args.push(OsString::from("--share-net"));
    }

    // 最小只读系统树（运行 sh 及常用工具所需）。
    for path in SYSTEM_RO_DIRS {
        args.push(OsString::from("--ro-bind-try"));
        args.push(OsString::from(path));
        args.push(OsString::from(path));
    }
    // 精确的运行时 `/etc` 例外。不要 bind 整个 `/etc`：宿主装配的默认
    // filesystem policy 仅授权工作区，完整 `/etc` 会让 shell 绕过它。
    for path in SYSTEM_RO_FILES {
        args.push(OsString::from("--ro-bind-try"));
        args.push(OsString::from(path));
        args.push(OsString::from(path));
    }
    args.push(OsString::from("--ro-bind-try"));
    args.push(OsString::from(SYSTEM_RO_CERT_DIR));
    args.push(OsString::from(SYSTEM_RO_CERT_DIR));
    // /proc 与 /dev（受限）、/tmp 隔离为 tmpfs。
    args.push(OsString::from("--proc"));
    args.push(OsString::from("/proc"));
    args.push(OsString::from("--dev"));
    args.push(OsString::from("/dev"));
    args.push(OsString::from("--tmpfs"));
    args.push(OsString::from("/tmp"));

    // 文件系统四象限：绑定绝对、非 glob 的 allow_read（只读）/allow_write（读写）
    // 条目；glob 模式与 deny_* 在 Layer 1 权限引擎对文件工具精确执行，
    // 此处提供 shell 的最小可用可见面。
    for entry in &policy.filesystem.allow_read {
        if let Some(path) = bindable_path(entry) {
            args.push(OsString::from("--ro-bind-try"));
            args.push(path.clone());
            args.push(path);
        }
    }
    for entry in &policy.filesystem.allow_write {
        if let Some(path) = bindable_path(entry) {
            args.push(OsString::from("--bind-try"));
            args.push(path.clone());
            args.push(path);
        }
    }

    // 工作目录始终可写并作为 cwd。必须位于象限绑定**之后**：bwrap 按 argv
    // 顺序应用挂载，若 allow_read 含 cwd 的祖先目录，晚于 cwd 挂载的只读
    // 祖先绑定会遮蔽先前的可写 cwd 绑定（沙箱内 cwd 意外只读）；最后挂载
    // 可写 cwd 保证无论策略如何，工作目录恒可写（review 修复）。
    let cwd = request.cwd.as_os_str().to_os_string();
    args.push(OsString::from("--bind"));
    args.push(cwd.clone());
    args.push(cwd.clone());

    args.push(OsString::from("--chdir"));
    args.push(cwd);

    // 命令本体。
    args.push(OsString::from("--"));
    args.push(OsString::from("sh"));
    args.push(OsString::from("-c"));
    args.push(OsString::from(&request.command));
    args
}

/// 仅绑定绝对且不含 glob 元字符的路径条目（bwrap 需要具体路径）。
fn bindable_path(entry: &str) -> Option<OsString> {
    let trimmed = entry.trim();
    if trimmed.is_empty() || trimmed.contains(['*', '?', '[']) {
        return None;
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        Some(path.into_os_string())
    } else {
        None
    }
}

/// A bind mount is always readable as well as writable. When an explicit read
/// allowlist excludes an absolute writable bind, mounting it would widen the
/// shell's read surface beyond the four-quadrant policy. Refuse that policy
/// rather than silently treating its write grant as an implicit read grant.
fn has_write_bind_outside_read_policy(policy: &SandboxPolicy) -> bool {
    !policy.filesystem.allow_read.is_empty()
        && policy
            .filesystem
            .allow_write
            .iter()
            .any(|entry| bindable_path(entry).is_some() && !policy.filesystem.can_read(entry))
}

async fn run_bwrap(
    bwrap: &std::path::Path,
    policy: &SandboxPolicy,
    request: ShellRequest,
) -> Result<ShellOutcome, SandboxError> {
    use std::process::Stdio;

    let args = build_bwrap_args(policy, &request);
    let mut command = tokio::process::Command::new(bwrap);
    command
        .args(&args)
        // Never inherit the host process environment: the desktop client keeps
        // AINS_API_TOKEN there.  Only provide a minimal command environment
        // inside the isolated namespace.
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("HOME", "/tmp")
        .env("TMPDIR", "/tmp")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // 独立进程组：超时/取消时 killpg 可连 sh 及其孙进程一并终止
    // （kill_on_drop 只杀 bwrap 本身，pid namespace 内的命令会残留）。
    command.process_group(0);

    let cap = request.max_output_bytes;
    let timeout = request.timeout;
    let cancel = request.cancel.clone();
    let stdout_sink = request.output_sink.clone();
    let stderr_sink = request.output_sink.clone();
    let mut child = command
        .spawn()
        .map_err(|error| SandboxError::Execution(format!("spawn bwrap failed: {error}")))?;
    let child_pid = child.id().unwrap_or(0);
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();

    // 读管道时即强制字节上限（不先无界收集再截断）；超限剩余由内核管道
    // 背压 + 超时/取消分支 killpg 兜底（阻塞进程树被整组终止后 wait 收割）。
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
        // 终止整个进程组（bwrap + sh + 孙进程），再 wait 收割避免僵尸；
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
    let status =
        status.map_err(|error| SandboxError::Execution(format!("wait bwrap failed: {error}")))?;
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
    use crate::policy::sandbox_policy::{FilesystemPolicy, NetworkPolicy};
    use std::sync::Arc;
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

    fn os(values: &[OsString]) -> Vec<String> {
        values
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn args_isolate_by_default_and_bind_cwd() {
        let args = os(&build_bwrap_args(
            &SandboxPolicy::default(),
            &request("echo hi"),
        ));
        assert!(args.contains(&"--unshare-all".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--new-session".to_string()));
        // 空策略（无 deny "*"）→ 共享网络
        assert!(args.contains(&"--share-net".to_string()));
        // cwd 绑定为可写并 chdir
        assert!(
            args.windows(3)
                .any(|w| w == ["--bind", "/work/project", "/work/project"])
        );
        assert!(args.windows(2).any(|w| w == ["--chdir", "/work/project"]));
        // `/etc` must not be broadly mounted: the default allowlist grants
        // only the workspace, so host configuration remains unavailable.
        assert!(
            !args
                .windows(3)
                .any(|w| w == ["--ro-bind-try", "/etc", "/etc"])
        );
        assert!(
            !args.iter().any(|entry| entry == "/etc/hostname"),
            "host identity must not be part of the runtime exception set"
        );
        // 命令以 -- sh -c <cmd> 收尾
        let tail = &args[args.len() - 4..];
        assert_eq!(tail, ["--", "sh", "-c", "echo hi"]);
    }

    #[test]
    fn cwd_writable_bind_follows_read_ancestor_binds() {
        // review 修复回归：cwd 的可写 --bind 必须位于 allow_read 祖先目录的
        // 只读绑定**之后**。bwrap 按 argv 顺序应用挂载，若祖先只读绑定晚于
        // cwd 挂载，会遮蔽 cwd 的可写性（沙箱内 cwd 意外只读）。
        // 触发策略：allow_read 含 cwd 祖先（/work ⊇ /work/project）而
        // allow_write 为空（空白名单默认放行，exec_shell 的
        // shell_cwd_is_allowed 检查不会拦截）。
        let policy = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec!["/work".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let args = os(&build_bwrap_args(&policy, &request("ls")));

        let ro_ancestor = args
            .windows(3)
            .position(|w| w == ["--ro-bind-try", "/work", "/work"])
            .expect("allow_read ancestor must be bound read-only");
        let cwd_bind = args
            .windows(3)
            .position(|w| w == ["--bind", "/work/project", "/work/project"])
            .expect("cwd must be bound writable");
        assert!(
            cwd_bind > ro_ancestor,
            "cwd writable bind ({cwd_bind}) must follow ancestor ro-bind ({ro_ancestor}) \
             or the ancestor shadows cwd's writability"
        );

        // allow_write 显式含 cwd 时同样成立（循环绑定后仍有最终可写覆盖）。
        let explicit = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec!["/work".into()],
                allow_write: vec!["/work/project".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let args = os(&build_bwrap_args(&explicit, &request("ls")));
        let last_cwd_bind = args
            .windows(3)
            .rposition(|w| w == ["--bind", "/work/project", "/work/project"])
            .expect("cwd must be bound writable");
        let last_ro_ancestor = args
            .windows(3)
            .rposition(|w| w == ["--ro-bind-try", "/work", "/work"])
            .expect("allow_read ancestor must be bound read-only");
        assert!(
            last_cwd_bind > last_ro_ancestor,
            "final cwd writable bind ({last_cwd_bind}) must follow ancestor ro-bind ({last_ro_ancestor})"
        );
    }

    #[test]
    fn deny_all_network_omits_share_net() {
        let policy = SandboxPolicy {
            network: NetworkPolicy {
                allowed_domains: vec![],
                denied_domains: vec!["*".into()],
            },
            ..Default::default()
        };
        let args = os(&build_bwrap_args(&policy, &request("curl https://x")));
        assert!(
            !args.contains(&"--share-net".to_string()),
            "全断网络应无 --share-net"
        );
    }

    #[test]
    fn allow_write_binds_absolute_dirs_only() {
        let policy = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_write: vec!["/data".into(), "relative/skip".into(), "/glob/*".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let args = os(&build_bwrap_args(&policy, &request("ls")));
        // 绝对非 glob 目录被绑定
        assert!(
            args.windows(3)
                .any(|w| w == ["--bind-try", "/data", "/data"])
        );
        // 相对路径与 glob 跳过
        assert!(!args.iter().any(|a| a.contains("relative")));
        assert!(!args.iter().any(|a| a.contains("/glob")));
    }

    #[test]
    fn missing_bwrap_reports_shell_unavailable() {
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(SandboxPolicy::default(), None);
        assert!(!sandbox.capabilities().shell);
    }

    #[test]
    fn smoke_test_detects_failing_bwrap_binary() {
        // 冒烟探测回归："bwrap 存在但无法创建 namespace"（模拟：脚本非零
        // 退出）必须判定为不可用（fail-closed），不得让 capabilities().shell
        // 误报 true。
        let dir = tempfile::tempdir().unwrap();
        let failing = dir.path().join("bwrap-fail");
        std::fs::write(&failing, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(
            !smoke_test_bwrap(&failing),
            "非零退出的 bwrap 必须视为不可用"
        );

        let ok_script = dir.path().join("bwrap-ok");
        std::fs::write(&ok_script, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ok_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(smoke_test_bwrap(&ok_script), "零退出应判定可用");

        // 不存在/不可执行的文件：不可用（不会 panic）。
        assert!(!smoke_test_bwrap(&dir.path().join("no-such-bwrap")));
    }

    #[tokio::test]
    async fn missing_bwrap_refuses_execution_without_running() {
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(SandboxPolicy::default(), None);
        let result = sandbox.exec_shell(request("touch should-not-run")).await;
        match result {
            Err(SandboxError::Unavailable(reason)) => {
                assert!(reason.contains("无操作权限"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_refuses_unrepresentable_fine_grained_policy() {
        let policy = SandboxPolicy {
            network: NetworkPolicy {
                allowed_domains: vec!["example.com".into()],
                ..Default::default()
            },
            filesystem: FilesystemPolicy {
                deny_write: vec!["/work/secrets/*".into()],
                ..Default::default()
            },
        };
        let sandbox =
            LinuxBubblewrapSandbox::with_bwrap(policy, Some(PathBuf::from("/usr/bin/bwrap")));
        // `exec_shell` canonicalizes cwd before it evaluates policy fidelity;
        // use a real directory so this test reaches the intended fail-closed
        // policy branch instead of failing on an unrelated fixture path.
        let cwd = tempfile::tempdir().unwrap();
        let mut req = request("echo hi");
        req.cwd = cwd.path().to_path_buf();
        let result = sandbox.exec_shell(req).await;
        assert!(
            matches!(result, Err(SandboxError::Unavailable(reason)) if reason.contains("cannot faithfully enforce"))
        );
    }

    #[tokio::test]
    async fn shell_refuses_writable_bind_outside_read_allowlist() {
        // bwrap 的 --bind 同时授予读和写；若可写目录不在显式读白名单内，
        // 继续执行会把“只写”策略悄悄扩大为可读，必须在 spawn 前 fail-closed。
        let workspace = tempfile::tempdir().unwrap();
        let write_only = tempfile::tempdir().unwrap();
        let workspace_path = workspace.path().display().to_string();
        let policy = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec![workspace_path.clone()],
                allow_write: vec![workspace_path, write_only.path().display().to_string()],
                ..Default::default()
            },
            ..Default::default()
        };
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(
            policy,
            Some(PathBuf::from("/path/that-must-not-run-bwrap")),
        );
        assert!(!sandbox.capabilities().filesystem_policy);

        let mut req = request("echo hi");
        req.cwd = workspace.path().to_path_buf();
        assert!(matches!(
            sandbox.exec_shell(req).await,
            Err(SandboxError::Unavailable(reason)) if reason.contains("cannot faithfully enforce")
        ));
    }

    #[tokio::test]
    async fn root_cwd_is_refused_by_backend_guard() {
        // B1 纵深防御回归：即使 bwrap 可用，cwd 为文件系统根也必须拒绝——
        // `--bind / /` 可写绑定使整个文件系统在沙箱内可写（沙箱即宿主），
        // 与"沙箱内破坏命令只破坏沙箱视图"的假设矛盾。
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(
            SandboxPolicy::default(),
            Some(PathBuf::from("/usr/bin/bwrap")),
        );
        let mut req = request("echo hi");
        req.cwd = PathBuf::from("/");
        match sandbox.exec_shell(req).await {
            Err(SandboxError::Unavailable(reason)) => {
                assert!(reason.contains("filesystem root"), "{reason}");
            }
            other => panic!("expected Unavailable for root cwd, got {other:?}"),
        }
        // 规范化后才成为根目录的路径也必须拒绝，否则会绕过前置的
        // `parent() == None` 检查并最终构造 `--bind / /`。
        let mut lexical_root = request("echo hi");
        lexical_root.cwd = PathBuf::from("/tmp/..");
        match sandbox.exec_shell(lexical_root).await {
            Err(SandboxError::Unavailable(reason)) => {
                assert!(reason.contains("filesystem root"), "{reason}");
            }
            other => panic!("expected Unavailable for lexical root cwd, got {other:?}"),
        }
        // 非根 cwd（即使不存在）不受此守卫影响：交给 bwrap 路径正常处理。
        let mut ok_req = request("true");
        ok_req.cwd = PathBuf::from("/work/project");
        // bwrap 不存在时返回 Unavailable（bwrap 缺失路径），而非 root 守卫。
        match LinuxBubblewrapSandbox::with_bwrap(SandboxPolicy::default(), None)
            .exec_shell(ok_req)
            .await
        {
            Err(SandboxError::Unavailable(reason)) => {
                assert!(reason.contains("未检测到 bubblewrap"), "{reason}");
            }
            other => panic!("expected bwrap-missing Unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_refuses_cwd_excluded_by_filesystem_allowlist() {
        // A shell cwd is a writable bwrap bind, so an allowlist that excludes
        // it must reject before attempting to execute the configured bwrap.
        let policy = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec!["/allowed".into()],
                allow_write: vec!["/allowed".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let cwd = tempfile::tempdir().unwrap();
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(
            policy,
            Some(PathBuf::from("/path/that-must-not-run-bwrap")),
        );
        let mut req = request("echo hi");
        req.cwd = cwd.path().to_path_buf();
        let result = sandbox.exec_shell(req).await;
        assert!(matches!(
            result,
            Err(SandboxError::Unavailable(reason)) if reason.contains("cwd is blocked")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_resolves_cwd_before_enforcing_filesystem_policy() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let escape = workspace.path().join("escape");
        symlink("/", &escape).unwrap();
        let policy = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec![workspace.path().display().to_string()],
                allow_write: vec![workspace.path().display().to_string()],
                ..Default::default()
            },
            ..Default::default()
        };
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(
            policy,
            Some(PathBuf::from("/path/that-must-not-run-bwrap")),
        );
        let mut req = request("echo hi");
        req.cwd = escape;
        let result = sandbox.exec_shell(req).await;
        assert!(matches!(
            result,
            Err(SandboxError::Unavailable(reason)) if reason.contains("filesystem root")
        ));
    }

    // 真实 bwrap 隔离验证：仅在环境安装了 bwrap 时运行，否则跳过。
    #[tokio::test]
    async fn real_bwrap_runs_and_isolates_filesystem_when_available() {
        let Some(bwrap) = which_bwrap() else {
            eprintln!("skipping: bubblewrap (bwrap) not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::default();
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(policy, Some(bwrap));
        // cwd 内可写
        let req = ShellRequest {
            command: "echo sandboxed > out.txt && cat out.txt".into(),
            cwd: dir.path().to_path_buf(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
            cancel: None,
            output_sink: None,
        };
        let outcome = sandbox.exec_shell(req).await;
        // bwrap 在部分 CI 环境（无 user namespace 权限）会失败；此时不断言隔离，
        // 仅确认调用路径不 panic 且返回结构化结果。
        if let Ok(outcome) = outcome
            && outcome.exit_code == Some(0)
        {
            assert!(outcome.output.contains("sandboxed"));
            // cwd 绑定可写：文件应落在真实 cwd
            assert!(dir.path().join("out.txt").exists());
        }
    }

    // 真实 bwrap 输出超限回归：命令输出超过 max_output_bytes 后写端阻塞挂起，
    // 超时仍必须终止并返回（修复前：读取阶段 select 结束即丢弃 timeout future，
    // wait() 无限挂起）。外层 tokio timeout 保护测试框架防挂死。
    #[tokio::test]
    async fn output_over_cap_still_times_out_when_available() {
        let Some(bwrap) = which_bwrap() else {
            eprintln!("skipping: bubblewrap (bwrap) not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(SandboxPolicy::default(), Some(bwrap));
        let req = ShellRequest {
            // 持续输出远超 4096 上限：读满 cap 后写端阻塞，命令挂起。
            command: "head -c 100000 /dev/zero".into(),
            cwd: dir.path().to_path_buf(),
            timeout: Duration::from_secs(1),
            max_output_bytes: 4096,
            cancel: None,
            output_sink: None,
        };
        let outcome =
            match tokio::time::timeout(Duration::from_secs(5), sandbox.exec_shell(req)).await {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(e)) => {
                    eprintln!("skipping: bwrap cannot run here: {e}");
                    return;
                }
                Err(_) => panic!("exec_shell hung: output-over-cap command never timed out"),
            };
        assert!(outcome.timed_out, "expected timed_out for blocked writer");
    }

    #[test]
    fn kill_zero_pid_is_noop() {
        // review 修复回归：child_pid=0 必须被守卫拦截。若未拦截，kill(0, SIGKILL)
        // 会向**本测试进程所在进程组**发 SIGKILL（含测试进程自身）——
        // 测试进程将直接死亡，cargo test 必然失败，从而验证守卫存在。
        crate::policy::sandbox::kill_process_group(0);
    }

    #[test]
    fn kill_oversize_pid_is_noop() {
        // 超出 i32 范围的 pid：`-(child_pid as i32)` 会溢出（debug panic）或
        // 命中错误进程（release 回绕）；守卫必须拦截，不得 panic。
        crate::policy::sandbox::kill_process_group(i32::MAX as u32 + 1);
        crate::policy::sandbox::kill_process_group(u32::MAX);
    }

    #[test]
    fn kill_unknown_pgid_is_ignored() {
        // 有效范围内但不存在的进程组：kill(2) 返回 ESRCH，静默忽略（不 panic）。
        // 2^30 是合法 i32 且现实中无对应 pgid。
        crate::policy::sandbox::kill_process_group(1 << 30);
    }

    // 真实 bwrap 超时终止验证：timeout 触发后必须杀掉 sh 及其孙进程
    // （kill_on_drop 只杀 bwrap，历史缺陷：命令超时后继续在沙箱内运行）。
    #[tokio::test]
    async fn timeout_kills_entire_process_tree_when_available() {
        let Some(bwrap) = which_bwrap() else {
            eprintln!("skipping: bubblewrap (bwrap) not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(SandboxPolicy::default(), Some(bwrap));
        let req = ShellRequest {
            command: "echo started > marker; sleep 5; echo finished >> marker".into(),
            cwd: dir.path().to_path_buf(),
            timeout: Duration::from_secs(1),
            max_output_bytes: 4096,
            cancel: None,
            output_sink: None,
        };
        let outcome = match sandbox.exec_shell(req).await {
            Ok(outcome) => outcome,
            Err(e) => {
                // 无 user namespace 权限等环境限制：跳过（不弱化断言）。
                eprintln!("skipping: bwrap cannot run here: {e}");
                return;
            }
        };
        assert!(outcome.timed_out, "expected timeout outcome");
        assert!(
            !outcome.cancelled,
            "timeout-triggered termination must not be marked cancelled"
        );
        // 等过 sleep 原定结束时刻；若进程树未被终止，marker 会出现 "finished"。
        tokio::time::sleep(Duration::from_secs(6)).await;
        let content = std::fs::read_to_string(dir.path().join("marker")).unwrap_or_default();
        assert!(
            !content.contains("finished"),
            "process tree survived timeout: {content:?}"
        );
        assert!(
            content.contains("started"),
            "command never started: {content:?}"
        );
    }

    // 真实 bwrap 协作式取消验证：cancel 置位后同样整树终止（与超时共享路径）。
    #[tokio::test]
    async fn cancel_flag_kills_entire_process_tree_when_available() {
        use std::sync::atomic::Ordering;

        let Some(bwrap) = which_bwrap() else {
            eprintln!("skipping: bubblewrap (bwrap) not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let sandbox = LinuxBubblewrapSandbox::with_bwrap(SandboxPolicy::default(), Some(bwrap));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let req = ShellRequest {
            command: "echo started > marker; sleep 5; echo finished >> marker".into(),
            cwd: dir.path().to_path_buf(),
            // 长超时：确保由 cancel（而非 timeout）触发终止。
            timeout: Duration::from_secs(60),
            max_output_bytes: 4096,
            cancel: Some(Arc::clone(&cancel)),
            output_sink: None,
        };
        let handle = tokio::spawn(async move { sandbox.exec_shell(req).await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.store(true, Ordering::SeqCst);
        let outcome = match handle.await.expect("exec task panicked") {
            Ok(outcome) => outcome,
            Err(e) => {
                eprintln!("skipping: bwrap cannot run here: {e}");
                return;
            }
        };
        assert!(outcome.timed_out, "cancel must yield timed_out outcome");
        assert!(
            outcome.cancelled,
            "cancel-triggered termination must be marked cancelled"
        );
        // 等过 sleep 原定结束时刻；若进程树未被终止，marker 会出现 "finished"。
        tokio::time::sleep(Duration::from_secs(6)).await;
        let content = std::fs::read_to_string(dir.path().join("marker")).unwrap_or_default();
        assert!(
            !content.contains("finished"),
            "process tree survived cancel: {content:?}"
        );
    }
}
