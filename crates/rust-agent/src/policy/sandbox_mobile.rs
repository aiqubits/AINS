//! Mobile 平台 Sandbox（Phase 7.1 Layer 2）：Android/iOS 应用沙箱（环境自带）。
//!
//! **与 Desktop 根本不同**：移动 OS 已把整个应用置于强隔离沙箱中
//! （Android：per-UID + SELinux 域 + zygote seccomp；iOS：容器沙箱 + 代码
//! 签名，且内核**禁止 fork/exec 任意二进制**）。因此：
//! - 没有可供我们"构建隔离"的子进程——OS 提供的隔离强于进程内自建；
//! - iOS 平台设计禁止 shell（exec 被内核阻止）；Android 应用策略/实践亦
//!   不支持有用地派生 shell。
//!
//! 故 shell **恒不可用**（不同于 mac/Win 的 opt-in：移动端根本没有可启用的
//! 沙箱化 shell 机制——启用只会 iOS 硬失败或 Android 无沙箱直跑，违背默认
//! 拒绝原则）。文件系统四象限与网络域策略在移动端由 **Layer 1**
//! （`PermissionEngine` 四象限 + `web_fetch` DomainRule）强制。
//!
//! **未在真实 Android/iOS 环境验证**：本适配为 FFI-free 语义层（无平台特定
//! API，结构等价 `NoopSandbox`，编译由构造保证）；但整体移动 agent 集成
//! （OS 应用沙箱 + Layer 1 策略在真机的实际行为）尚未验证，且 `app/mobile`
//! 目前未接入 rust-agent（本适配为前瞻基础设施，接入后即生效）。

use crate::policy::sandbox::{
    Sandbox, SandboxCapabilities, SandboxError, ShellOutcome, ShellRequest,
};
use crate::policy::sandbox_policy::SandboxPolicy;

/// 平台名（诊断/日志用）。
#[cfg(target_os = "android")]
const PLATFORM_NAME: &str = "android-app-sandbox";
#[cfg(target_os = "ios")]
const PLATFORM_NAME: &str = "ios-app-sandbox";

/// 移动端 Sandbox：识别 OS 应用沙箱为隔离边界，正确地不提供子进程 shell。
/// 文件/网络策略由 Layer 1 执行（本层不做 Layer 2 shell 隔离）。
pub struct MobileSandbox {
    // 保留策略句柄以备将来（如 Android isolated-process 桥接）；当前文件/
    // 网络策略由 Layer 1 消费，此处不使用。
    #[allow(dead_code)]
    policy: SandboxPolicy,
}

impl MobileSandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait::async_trait]
impl Sandbox for MobileSandbox {
    fn name(&self) -> &'static str {
        PLATFORM_NAME
    }

    fn capabilities(&self) -> SandboxCapabilities {
        // 移动端不提供子进程 shell（平台设计）；Layer 2 无文件/网络策略执行
        //（由 Layer 1 承担）→ 能力全无（恒拒绝 shell）。
        SandboxCapabilities::default()
    }

    async fn exec_shell(&self, _request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
        Err(SandboxError::Unavailable(
            "无操作权限：移动端（Android/iOS）不提供子进程 shell——OS 应用沙箱即隔离边界，\
             iOS 内核禁止 fork/exec、Android 应用策略不支持有用地派生 shell；不同于桌面，\
             此处无可启用的沙箱化 shell（文件/网络策略由 Layer 1 权限引擎与 web_fetch 强制）"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    #[tokio::test]
    async fn mobile_never_offers_shell_and_refuses_by_platform_design() {
        // 移动端 shell 恒不可用（非 opt-in）：capabilities().shell=false，
        // exec_shell 恒拒绝（OS 应用沙箱是隔离边界；无可启用的沙箱化 shell）。
        let sandbox = MobileSandbox::new(SandboxPolicy::default());
        assert!(!sandbox.capabilities().shell);
        let request = ShellRequest {
            command: "echo hi".into(),
            cwd: PathBuf::from("/data/local/tmp"),
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
