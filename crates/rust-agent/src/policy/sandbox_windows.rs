//! Windows 平台 Sandbox（Phase 7.1 Layer 2）：Job Object 进程隔离。
//!
//! Windows 内核无 Linux 的 namespace/seccomp；其对等的进程约束机制是
//! **Job Object**：将子进程纳入一个带限制的作业对象——kill-on-close
//! （作业句柄关闭即终止全部子进程，防止逃逸残留）、die-on-unhandled-exception，
//! 以及 UI 限制（拒绝访问剪贴板/桌面/句柄/全局原子等）。经 `windows-sys`
//! FFI 施加（无需安装，Win32 内建 API）。
//!
//! **未在真实 Windows 环境验证**：隔离代码已实现，但本机（Linux）无法运行
//! 验证。遵循默认拒绝原则，默认拒绝执行，仅当运维在 Windows 显式设置
//! `AINS_ENABLE_UNVERIFIED_SANDBOX=1` 后启用（见 [`unverified_sandbox_enabled`]）。
//!
//! 与 Linux/macOS 的差异（有意，记录在案）：Job Object 提供进程容器 +
//! 资源/UI 限制 + kill-on-close；更深的**受限令牌（CreateRestrictedToken）/
//! AppContainer** 权限剥离为后续增强项。文件系统四象限与网络域策略在本
//! 平台仍由 Layer 1 权限引擎（PermissionEngine 四象限）与 web_fetch 精确执行。
//!
//! 平台特定依赖（`tokio::process`、`windows-sys`）仅在本 cfg 门控文件出现。

use std::ffi::OsString;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_GLOBALATOMS,
    JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};

use crate::policy::sandbox::{
    Sandbox, SandboxCapabilities, SandboxError, ShellOutcome, ShellRequest,
    unverified_sandbox_enabled,
};
use crate::policy::sandbox_policy::SandboxPolicy;

/// Job Object 句柄 RAII 守卫：Drop 时 `CloseHandle`，触发 kill-on-close
/// 终止作业内全部子进程。`HANDLE`（原始指针）跨 `.await` 持有需 `Send`；
/// Win32 句柄为进程内全局值，跨线程传递安全，故显式声明。
struct JobHandle(HANDLE);

// SAFETY: Win32 内核对象句柄是进程范围的整数值，可安全跨线程移动/使用。
unsafe impl Send for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: 句柄由 CreateJobObjectW 成功返回且仅在此关闭一次。
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Windows Job Object 沙箱：策略在构造时固定。
pub struct WindowsJobObjectSandbox {
    policy: SandboxPolicy,
}

impl WindowsJobObjectSandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait::async_trait]
impl Sandbox for WindowsJobObjectSandbox {
    fn name(&self) -> &'static str {
        "windows-job-object"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        // Job Object 恒可用（Win32 内建），但隔离未经真实环境验证 → 默认关闭
        // （fail-closed），仅 opt-in 后启用。Job Object 尚未实现受限令牌 /
        // AppContainer，因此对任何需要网络或文件系统规则的 shell 策略必须
        // 保持关闭；Layer 1 不能从任意 shell 文本可靠恢复实际访问路径。
        if unverified_sandbox_enabled() && !self.policy.requires_shell_policy_enforcement() {
            SandboxCapabilities {
                shell: true,
                network_policy: false,
                filesystem_policy: false,
            }
        } else {
            SandboxCapabilities::default()
        }
    }

    async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
        // B1 纵深防御：cwd 为文件系统根（如 `C:\`）时拒绝（cmd 以 cwd 启动，
        // 根目录即全盘可写）。工具层已约束 cwd ⊆ 工作区；本检查保护绕过
        // 工具层的直接调用方。`parent() == None` 即根目录（Windows "C:\"）。
        if request.cwd.parent().is_none() {
            return Err(SandboxError::Unavailable(
                "refusing to run shell with cwd at filesystem root: the child process \
                 would start at a writable root"
                    .to_string(),
            ));
        }
        if !unverified_sandbox_enabled() {
            return Err(SandboxError::Unavailable(
                "无操作权限：Windows Job Object 隔离已实现但未在真实环境验证，默认拒绝执行 \
                 shell（在 Windows 上设置 AINS_ENABLE_UNVERIFIED_SANDBOX=1 显式启用以验证；\
                 遵循默认拒绝原则不降级直跑）"
                    .to_string(),
            ));
        }
        if self.policy.requires_shell_policy_enforcement() {
            return Err(SandboxError::Unavailable(
                "Windows Job Object sandbox cannot enforce this shell network or filesystem policy; refusing execution until restricted-token/AppContainer isolation is available"
                    .to_string(),
            ));
        }
        run_in_job(request).await
    }
}

/// 创建带限制的 Job Object：kill-on-close + die-on-unhandled-exception +
/// UI 限制（拒绝剪贴板/桌面/句柄/全局原子访问）。失败返回 `None`。
fn create_confined_job() -> Option<JobHandle> {
    // SAFETY: 传 null 属性/名称创建匿名作业；成功返回非空句柄。
    let handle = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
    if handle.is_null() {
        return None;
    }
    let job = JobHandle(handle);

    // 扩展限制：kill-on-close（作业关闭即杀全部子进程）+ 未处理异常直接终止。
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    // SAFETY: 传入正确的信息类与结构体大小。
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return None;
    }

    // UI 限制：拒绝读写剪贴板、桌面切换、句柄继承、全局原子等。
    let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = unsafe { core::mem::zeroed() };
    ui.UIRestrictionsClass = JOB_OBJECT_UILIMIT_READCLIPBOARD
        | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
        | JOB_OBJECT_UILIMIT_DESKTOP
        | JOB_OBJECT_UILIMIT_HANDLES
        | JOB_OBJECT_UILIMIT_GLOBALATOMS;
    // SAFETY: 同上，UI 限制信息类。
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectBasicUIRestrictions,
            (&raw const ui).cast(),
            core::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        )
    };
    if ok == 0 {
        // UI 限制施加失败，不创建不完整的 Job Object（缺 UI 隔离仍可能
        // 泄漏剪贴板 / 桌面访问，违背 fail-closed 原则）。
        return None;
    }
    Some(job)
}

async fn run_in_job(request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
    use std::process::Stdio;

    let Some(job) = create_confined_job() else {
        return Err(SandboxError::Execution(
            "创建 Job Object 失败，拒绝在无隔离下执行 shell".to_string(),
        ));
    };

    // 只接受 System32 的绝对 cmd.exe 路径。SystemRoot 缺失时不能回退到
    // 裸 `cmd`（那会重新经 PATH 搜索，可能执行攻击者植入的可执行文件）。
    let system_root = std::env::var_os("SystemRoot");
    let cmd = system_cmd_path(system_root.clone())?;
    let mut command = tokio::process::Command::new(&cmd);
    command
        .arg("/C")
        .arg(&request.command)
        .current_dir(&request.cwd)
        // Do not inherit desktop credentials.  Preserve only the Windows
        // loader variables needed by cmd and a minimal command path.
        .env_clear()
        .env("SystemRoot", system_root.unwrap_or_default())
        .env(
            "SystemDrive",
            std::env::var_os("SystemDrive").unwrap_or_default(),
        )
        .env("Path", r"%SystemRoot%\System32;%SystemRoot%")
        .env("TEMP", r"%SystemRoot%\Temp")
        .env("TMP", r"%SystemRoot%\Temp")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let cap = request.max_output_bytes;
    let timeout = request.timeout;
    let cancel = request.cancel.clone();
    let stdout_sink = request.output_sink.clone();
    let stderr_sink = request.output_sink.clone();
    // job 守卫贯穿整个执行；结束后 Drop→CloseHandle 触发 kill-on-close，
    // 杀掉任何残留子进程（防逃逸）。
    let mut child = command
        .spawn()
        .map_err(|error| SandboxError::Execution(format!("spawn cmd failed: {error}")))?;
    // 立即将子进程纳入作业（spawn 到 assign 之间有极小竞态窗口，
    // 仅 cmd 加载器初始指令，受限令牌/挂起创建为后续增强项）。
    match child.raw_handle() {
        Some(handle) => {
            // SAFETY: handle 为有效进程句柄，job.0 为有效作业句柄。
            let ok = unsafe { AssignProcessToJobObject(job.0, handle as HANDLE) };
            if ok == 0 {
                // 子进程未被纳入作业：立即杀掉并拒绝在无隔离下继续执行。
                let _ = child.start_kill();
                return Err(SandboxError::Execution(
                    "AssignProcessToJobObject failed; refusing to run without isolation".into(),
                ));
            }
        }
        None => {
            // spawn 成功后理论不可达（进程句柄缺失）；仍 fail-closed：
            // 不在无 Job 隔离下继续执行（否则子进程逃逸隔离边界）。
            let _ = child.start_kill();
            return Err(SandboxError::Execution(
                "child process handle unavailable; refusing to run without isolation".into(),
            ));
        }
    }
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();

    // 读管道（有界）与超时/取消竞争；终止路径统一关闭作业句柄触发
    // kill-on-close（作业内全部子进程被杀，含 cmd 派生的孙进程）。
    // 超时/取消 future 在阶段间复用：**管道读完不等于进程退出**——输出超限后
    // 命令可能写端阻塞继续运行，wait 阶段仍需同一超时/取消兑底（回归修复：
    // 历史实现把 timeout 限制在读取阶段，超限命令会使 wait 无限挂起）。
    let timeout_fut = tokio::time::sleep(timeout);
    let cancel_fut = crate::policy::sandbox::wait_cancel(cancel);
    tokio::pin!(timeout_fut, cancel_fut);

    // 阶段 1：有界读管道（共享预算：合并输出 ≤ cap），与超时/取消竞争。
    // 预算耗尽后写端阻塞的剩余输出由终止路径兜底（start_kill + kill-on-close
    // 终止作业内全部子进程，含 cmd 派生的孙进程）。
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
        // 先杀 cmd（kill_on_drop 在 drop 时触发），再关闭作业句柄
        // kill-on-close 清理残留；wait 收割避免僵尸。
        let _ = child.start_kill();
        let _ = child.wait().await;
        drop(job); // kill-on-close：作业内残留子进程全部终止
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
            let _ = child.start_kill();
            let _ = child.wait().await;
            drop(job); // kill-on-close：作业内残留子进程全部终止
            return Ok(ShellOutcome {
                output: crate::policy::sandbox::merge_shell_output(&out_buf, &err_buf),
                exit_code: None,
                timed_out: true,
                cancelled: false,
            });
        }
        _ = &mut cancel_fut => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            drop(job); // kill-on-close：作业内残留子进程全部终止
            return Ok(ShellOutcome {
                output: crate::policy::sandbox::merge_shell_output(&out_buf, &err_buf),
                exit_code: None,
                timed_out: true,
                cancelled: true,
            });
        }
    };
    let status =
        status.map_err(|error| SandboxError::Execution(format!("wait cmd failed: {error}")))?;
    drop(job); // 显式：关闭作业句柄 → kill-on-close 清理残留
    Ok(ShellOutcome {
        output: crate::policy::sandbox::merge_shell_output(&out_buf, &err_buf),
        exit_code: status.code(),
        timed_out: false,
        cancelled: false,
    })
}

fn system_cmd_path(system_root: Option<OsString>) -> Result<PathBuf, SandboxError> {
    let root = system_root.filter(|root| !root.is_empty()).ok_or_else(|| {
        SandboxError::Unavailable(
            "SystemRoot is unavailable; refusing PATH-based cmd.exe lookup".to_string(),
        )
    })?;
    let cmd = PathBuf::from(root).join("System32").join("cmd.exe");
    if !cmd.is_absolute() {
        return Err(SandboxError::Unavailable(
            "SystemRoot is not an absolute path; refusing cmd.exe lookup".to_string(),
        ));
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn cmd_path_requires_an_absolute_system_root() {
        assert!(system_cmd_path(None).is_err());
        assert!(system_cmd_path(Some(OsString::from("relative-root"))).is_err());
        assert_eq!(
            system_cmd_path(Some(OsString::from(r"C:\Windows"))).unwrap(),
            PathBuf::from(r"C:\Windows")
                .join("System32")
                .join("cmd.exe")
        );
    }

    #[tokio::test]
    async fn default_without_opt_in_reports_shell_unavailable_and_refuses() {
        // 默认（未设 AINS_ENABLE_UNVERIFIED_SANDBOX）：未验证隔离 fail-closed 拒绝。
        let sandbox = WindowsJobObjectSandbox::new(SandboxPolicy::default());
        if !unverified_sandbox_enabled() {
            assert!(!sandbox.capabilities().shell);
            let request = ShellRequest {
                command: "echo hi".into(),
                cwd: PathBuf::from("C:\\Temp"),
                timeout: Duration::from_secs(1),
                max_output_bytes: 1024,
                cancel: None,
                output_sink: None,
            };
            assert!(matches!(
                sandbox.exec_shell(request).await,
                Err(SandboxError::Unavailable(_))
            ));
        }
    }
}
