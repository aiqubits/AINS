//! 三态权限引擎（对齐 OpenHarness `permissions/checker.py` + `modes.py`）。
//!
//! 决策序（修正基线已知缺陷）：
//! 内置敏感路径黑名单（最高优先级、不可覆盖）→ 工具显式 deny → 命令 deny
//! 模式 → PathRule glob 规则 → 工具显式 allow → full_auto 放行 → 只读放行
//! → plan 模式拦截写操作 → default 模式要求用户确认。
//!
//! AINS 扩展（对齐矩阵已注记）：
//! - PathRule 实现**完整 allow/deny 语义**（基线 allow 分支为死代码）：
//!   规则按序求值，首个命中的规则生效，allow 命中即放行（跳过模式门控）。
//! - `always allow` 会话级放行集（对应 6.11 权限交互 UI 的"总是允许"）。

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fnmatch::fnmatch;
use crate::marker::MaybeSendSync;
use crate::policy::sandbox_policy::FilesystemPolicy;

/// 权限模式（对齐 `permissions/modes.py`；acceptEdits 等属子代理范畴，不在此）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    Plan,
    FullAuto,
}

/// 内置敏感路径黑名单：无论模式与用户配置均拒绝（防 prompt injection 定向
/// 读取凭据）。与基线 `SENSITIVE_PATH_PATTERNS` 一致，OpenHarness 自有凭据
/// 库两项替换为 AINS 对应路径（偏差记录见对齐清单）。
pub const SENSITIVE_PATH_PATTERNS: &[&str] = &[
    // SSH keys and config
    "*/.ssh/*",
    // AWS credentials
    "*/.aws/credentials",
    "*/.aws/config",
    // GCP credentials
    "*/.config/gcloud/*",
    // Azure credentials
    "*/.azure/*",
    // GPG keys
    "*/.gnupg/*",
    // Docker credentials
    "*/.docker/config.json",
    // Kubernetes credentials
    "*/.kube/config",
    // AINS own credential stores
    "*/.ains/credentials.json",
];

/// glob 路径规则（对齐基线 `PathRule`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRule {
    pub pattern: String,
    /// `true` = allow，`false` = deny。
    pub allow: bool,
}

/// 权限配置（对齐基线 `PermissionSettings` 中本引擎消费的字段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionSettings {
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    /// 命令 deny glob（如 `rm -rf /*`）。
    pub denied_commands: Vec<String>,
    pub path_rules: Vec<PathRule>,
}

/// 单次工具调用的决策结果（对齐基线 `PermissionDecision`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    pub allowed: bool,
    pub requires_confirmation: bool,
    pub reason: String,
}

impl PermissionDecision {
    fn allow(reason: impl Into<String>) -> Self {
        Self {
            allowed: true,
            requires_confirmation: false,
            reason: reason.into(),
        }
    }

    pub(crate) fn deny(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            requires_confirmation: false,
            reason: reason.into(),
        }
    }
}

/// 异步询问回调的用户答复（对齐 6.11：允许 / 总是允许 / 拒绝）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionReply {
    Allow,
    /// 本会话内后续同名工具直接放行。
    AlwaysAllow,
    Deny,
}

/// 待确认请求（推给 UI 的上下文）。
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub reason: String,
    /// 原始结构化参数，供宿主 UI 展示本次实际操作。宿主在跨进程传递或
    /// 记录日志前应按自身策略隐藏令牌、密码等敏感字段。
    pub tool_input: Value,
    /// 权限引擎实际求值的规范化路径（若该工具包含路径参数）。
    pub resolved_file_path: Option<String>,
    /// 权限引擎实际求值的命令（若该工具包含 command 参数）。
    pub command: Option<String>,
}

/// 异步询问回调（UI 弹窗），由宿主注入 ToolRuntime。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait PermissionPrompt: MaybeSendSync {
    /// Ask the user to authorize an operation.  A supplied cancellation flag
    /// means the surrounding query has been stopped; implementations must
    /// fail closed rather than enqueueing a stale prompt once they observe it.
    ///
    /// The flag is intentionally advisory (a prompt may already be visible),
    /// so [`ToolRuntime`](crate::tools::ToolRuntime) re-checks it after this
    /// future resolves before it executes the tool.
    async fn confirm(
        &self,
        request: &PermissionRequest,
        cancel: Option<Arc<AtomicBool>>,
    ) -> PermissionReply;
}

/// 权限引擎：规则不可变，模式与会话级放行集内部可变
/// （enter/exit_plan_mode 工具与"总是允许"答复经共享句柄写入）。
pub struct PermissionEngine {
    settings: PermissionSettings,
    /// 文件系统四象限策略（sandbox 策略；空 = 全放行，保持未配置时行为）。
    /// 与用户配置的 [`PathRule`] 叠加：任一 deny 即拒。
    filesystem: FilesystemPolicy,
    mode: RwLock<PermissionMode>,
    /// "总是允许"累积的会话级工具放行集（不持久化）。
    session_allowed: RwLock<HashSet<String>>,
}

impl PermissionEngine {
    pub fn new(mode: PermissionMode, settings: PermissionSettings) -> Arc<Self> {
        Self::with_filesystem_policy(mode, settings, FilesystemPolicy::default())
    }

    /// 带文件系统四象限策略构造（sandbox 策略层注入；空策略 = 全放行）。
    pub fn with_filesystem_policy(
        mode: PermissionMode,
        settings: PermissionSettings,
        filesystem: FilesystemPolicy,
    ) -> Arc<Self> {
        // 空/全空白 pattern 的规则丢弃（对齐基线加载告警语义）
        let mut settings = settings;
        settings.path_rules.retain(|rule| {
            let keep = !rule.pattern.trim().is_empty();
            if !keep {
                tracing::warn!("skipping path rule with empty pattern");
            }
            keep
        });
        for rule in &mut settings.path_rules {
            rule.pattern = rule.pattern.trim().to_string();
        }
        Arc::new(Self {
            settings,
            filesystem,
            mode: RwLock::new(mode),
            session_allowed: RwLock::new(HashSet::new()),
        })
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.read().expect("permission mode lock poisoned")
    }

    /// 切换权限模式（enter/exit_plan_mode 工具与 UI 模式切换器共用入口）。
    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.write().expect("permission mode lock poisoned") = mode;
    }

    /// "总是允许"答复：本会话内该工具后续直接放行。
    pub fn allow_for_session(&self, tool_name: &str) {
        self.session_allowed
            .write()
            .expect("session allowlist lock poisoned")
            .insert(tool_name.to_string());
    }

    /// Check an additional read performed by a compound tool (for example
    /// `edit_file`, which reads the old file before writing it).  This keeps
    /// the read quadrant from being bypassed by a write-oriented tool.
    pub fn file_access_allowed(&self, path: &str, is_read_only: bool) -> bool {
        if sensitive_path_pattern(path).is_some() {
            return false;
        }
        let permitted = if is_read_only {
            self.filesystem.can_read(path)
        } else {
            self.filesystem.can_write(path)
        };
        if !permitted {
            return false;
        }
        !self.settings.path_rules.iter().any(|rule| {
            !rule.allow
                && policy_match_paths(path)
                    .iter()
                    .any(|candidate| policy_path_matches(candidate, &rule.pattern))
        })
    }

    /// Recursive traversal tools cannot soundly enforce a deny subtree from a
    /// single root check.  Fail closed whenever deny rules exist; callers can
    /// later add a per-entry authorizer without weakening the boundary.
    pub fn recursive_read_allowed(&self, root: &str) -> bool {
        self.filesystem.deny_read.is_empty()
            && !self.settings.path_rules.iter().any(|rule| !rule.allow)
            && self.file_access_allowed(root, true)
    }

    /// 三态决策：返回 允许 / 询问（requires_confirmation）/ 拒绝。
    pub fn evaluate(
        &self,
        tool_name: &str,
        is_read_only: bool,
        file_path: Option<&str>,
        command: Option<&str>,
    ) -> PermissionDecision {
        // 1. 内置敏感路径防护：恒生效，任何模式/配置不可覆盖
        //    （Native 端语义；WASM 无本地文件系统天然豁免，逻辑仍编译一致）
        if let Some(path) = file_path
            && let Some(pattern) = sensitive_path_pattern(path)
        {
            return PermissionDecision::deny(format!(
                "Access denied: {path} is a sensitive credential path \
                 (matched built-in pattern '{pattern}')"
            ));
        }

        // 2. 工具显式 deny
        if self
            .settings
            .denied_tools
            .iter()
            .any(|denied| denied == tool_name)
        {
            return PermissionDecision::deny(format!("{tool_name} is explicitly denied"));
        }

        // 2.5 文件系统四象限（sandbox 策略）：敏感路径之后、PathRule 之前。
        //     空策略 = 全放行（no-op）；与用户 PathRule 叠加，任一 deny 即拒。
        //     读/写象限由 is_read_only 区分。
        if let Some(path) = file_path {
            let permitted = if is_read_only {
                self.filesystem.can_read(path)
            } else {
                self.filesystem.can_write(path)
            };
            if !permitted {
                return PermissionDecision::deny(format!(
                    "Access denied: {path} is outside the sandbox filesystem {} policy",
                    if is_read_only { "read" } else { "write" }
                ));
            }
        }

        // 3. 命令 deny glob：显式命令黑名单是不可被工具白名单、
        //    AlwaysAllow 或 shell cwd 的 PathRule allow 覆盖的安全边界。
        if let Some(command) = command {
            for pattern in &self.settings.denied_commands {
                if fnmatch(command, pattern) {
                    return PermissionDecision::deny(format!(
                        "Command matches deny pattern: {pattern}"
                    ));
                }
            }
        }

        // 3.5 敏感操作二次确认（Phase 7.2）：破坏性命令 / 隐私工具在后续
        //     任何放行路径（PathRule allow / allowed_tools / 会话级放行 /
        //     full_auto）之前求值。只提升确认要求，从不放宽权限（review 修复：
        //     历史实现把本检查放在 PathRule 之后，allow 规则命中 cwd 时
        //     短路返回，破坏性命令绕过二次确认）。
        let sensitive = sensitive_operation(tool_name, command);

        // 4. PathRule glob 规则：按序求值、首个命中生效。路径规则
        //    必须先于工具白名单，避免广义的 `write_file` 授权覆盖目录 deny。
        //    （AINS 扩展：完整 allow/deny 语义，基线 allow 分支为死代码）。
        //    外层必须是规则、内层是路径形态（review 二轮修复：形态在外层时，
        //    前置 deny 规则若仅命中次形态会被后置 allow 规则绕过）。
        if let Some(path) = file_path {
            for rule in &self.settings.path_rules {
                for candidate in policy_match_paths(path) {
                    if policy_path_matches(&candidate, &rule.pattern) {
                        if !rule.allow {
                            return PermissionDecision::deny(format!(
                                "Path {path} matches deny rule: {}",
                                rule.pattern
                            ));
                        }
                        // allow 规则命中：跳过模式门控直接放行；敏感操作除外
                        // （破坏性命令 / 隐私工具即使被路径规则显式放行也
                        // 必须二次确认——与 allowed_tools 分支同口径）。
                        if let Some(reason) = sensitive {
                            return PermissionDecision {
                                allowed: false,
                                requires_confirmation: true,
                                reason: reason.into(),
                            };
                        }
                        return PermissionDecision::allow(format!(
                            "Path {path} matches allow rule: {}",
                            rule.pattern
                        ));
                    }
                }
            }
        }

        // 5. 工具显式 allow（配置级），先做敏感操作门控：即使宿主显式授权
        //    破坏性 shell 命令也必须二次确认，与会话级"总是允许"对齐。
        //    敏感路径与 PathRule deny 仍不可被覆盖。
        if self
            .settings
            .allowed_tools
            .iter()
            .any(|allowed| allowed == tool_name)
        {
            if let Some(reason) = sensitive_operation(tool_name, command) {
                return PermissionDecision {
                    allowed: false,
                    requires_confirmation: true,
                    reason: reason.into(),
                };
            }
            return PermissionDecision::allow(format!("{tool_name} is explicitly allowed"));
        }

        // 5.5 敏感操作二次确认（Phase 7.2）：破坏性命令 / 隐私读取即使在
        //     full_auto 与会话级"总是允许"下亦强制确认（类比 exit_plan_mode
        //     的 full_auto 不可绕过）。仅提升确认要求，从不放宽权限。
        //     判定值已在 3.5 提前求值（PathRule 之前的放行路径同源复用）。

        // 会话级"总是允许"在 plan 模式下挂起；敏感操作覆盖会话放行。
        if self.mode() != PermissionMode::Plan
            && self
                .session_allowed
                .read()
                .expect("session allowlist lock poisoned")
                .contains(tool_name)
        {
            if let Some(reason) = sensitive {
                return PermissionDecision {
                    allowed: false,
                    requires_confirmation: true,
                    reason: reason.into(),
                };
            }
            return PermissionDecision::allow(format!("{tool_name} is allowed for this session"));
        }

        // 6. full_auto：全部放行；敏感操作除外（强制确认）。
        if self.mode() == PermissionMode::FullAuto {
            if let Some(reason) = sensitive {
                return PermissionDecision {
                    allowed: false,
                    requires_confirmation: true,
                    reason: reason.into(),
                };
            }
            return PermissionDecision::allow("Auto mode allows all tools");
        }

        // 7. 只读工具恒放行；敏感工具（隐私读取）除外——即使只读也必须确认
        //    （review 修复：历史实现只读快路径位于敏感门控之后，未来注册
        //    `is_read_only()=true` 的隐私工具会绕过强制确认；当前 clipboard /
        //    screenshot 恒 `is_read_only()=false` 受既有测试保护）。
        if is_read_only {
            if let Some(reason) = sensitive {
                return PermissionDecision {
                    allowed: false,
                    requires_confirmation: true,
                    reason: reason.into(),
                };
            }
            return PermissionDecision::allow("read-only tools are allowed");
        }

        // 8. plan 模式：退出属于放宽权限，必须显式确认；其余写操作拒绝。
        if self.mode() == PermissionMode::Plan {
            if tool_name == "exit_plan_mode" {
                return PermissionDecision {
                    allowed: false,
                    requires_confirmation: true,
                    reason: "Exiting plan mode requires user confirmation".into(),
                };
            }
            return PermissionDecision::deny(
                "Plan mode blocks mutating tools until the user exits plan mode",
            );
        }

        // 9. default 模式：写操作要求用户确认
        let mut reason = String::from(
            "Mutating tools require user confirmation in default mode. \
             Approve the prompt when asked, or run /permissions full_auto \
             if you want to allow them for this session.",
        );
        if let Some(hint) = bash_permission_hint(command) {
            reason.push(' ');
            reason.push_str(hint);
        }
        PermissionDecision {
            allowed: false,
            requires_confirmation: true,
            reason,
        }
    }
}

/// 返回路径命中的内置敏感规则。遍历型工具必须对每个实际访问的文件再次
/// 调用，不能只校验搜索根；例如 `.aws` 根本身不会命中精确的
/// `*/.aws/credentials` 规则。
pub(crate) fn sensitive_path_pattern(file_path: &str) -> Option<&'static str> {
    for candidate in policy_match_paths(file_path) {
        for &pattern in SENSITIVE_PATH_PATTERNS {
            if policy_path_matches(&candidate, pattern) {
                return Some(pattern);
            }
        }
    }
    None
}

/// 参与策略匹配的路径形态（对齐 `_policy_match_paths`）：目录根附加尾随 `/`
/// 让 `*/.ssh/*` / `/etc/*` 这类模式能命中目录本身（grep/glob 的 root 场景）。
fn policy_match_paths(file_path: &str) -> Vec<String> {
    // Treat both separators as path separators on every host. This is
    // conservative on Unix and prevents Windows credential paths from
    // bypassing slash-based built-in patterns.
    let portable = file_path.replace('\\', "/");
    let normalized = portable.trim_end_matches('/');
    if normalized.is_empty() {
        return vec![file_path.to_string()];
    }
    vec![normalized.to_string(), format!("{normalized}/")]
}

fn policy_path_matches(path: &str, pattern: &str) -> bool {
    let path = path.replace('\\', "/");
    let pattern = pattern.replace('\\', "/");
    #[cfg(target_os = "windows")]
    {
        return fnmatch(&path.to_lowercase(), &pattern.to_lowercase());
    }
    #[cfg(not(target_os = "windows"))]
    fnmatch(&path, &pattern)
}

/// 包安装/脚手架命令的确认提示补充（对齐 `_bash_permission_hint`）。
fn bash_permission_hint(command: Option<&str>) -> Option<&'static str> {
    let command = command?;
    let lowered = command.to_lowercase();
    const INSTALL_MARKERS: &[&str] = &[
        "npm install",
        "pnpm install",
        "yarn install",
        "bun install",
        "pip install",
        "uv pip install",
        "poetry install",
        "cargo install",
        "create-next-app",
        "npm create ",
        "pnpm create ",
        "yarn create ",
        "bun create ",
        "npx create-",
        "npm init ",
        "pnpm init ",
        "yarn init ",
    ];
    if INSTALL_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        Some(
            "Package installation and scaffolding commands change the workspace, \
             so they will not run automatically in default mode.",
        )
    } else {
        None
    }
}

/// 隐私敏感工具（无论读写均暴露用户隐私）：即使 full_auto 也强制确认。
const SENSITIVE_TOOLS: &[&str] = &["clipboard", "screenshot"];

/// 破坏性 / 不可逆 shell 命令模式（fnmatch，小写比较）：full_auto 下亦
/// 强制确认。宁可误报（多一次确认）也不错放。
///
/// **安全边界说明**：本模式表是启发式 UX 确认（可被变体绕过，如 `\rm`、
/// 多空格、参数重排）；它不是安全边界——shell 真正的安全边界是
/// Phase 7.1 Layer 2 平台沙箱（bwrap 容器内 `rm -rf /` 只破坏沙箱视图，
/// 且隔离不可用时 shell 整体拒绝执行）。不要依赖本表做安全裁决。
const SENSITIVE_COMMAND_PATTERNS: &[&str] = &[
    "*rm -rf*",
    "*rm -fr*",
    "*rm -r *",
    "*rm --recursive*",
    "sudo *",
    "*| sudo *",
    "* sudo *",
    "*mkfs*",
    "*dd if=*",
    "*of=/dev/*",
    "*> /dev/sd*",
    "*shutdown*",
    "*reboot*",
    "*chmod -r*",
    "*chown -r*",
    "*git push*--force*",
    "*git push*-f *",
    "*:(){*",
    "*curl*|*sh*",
    "*wget*|*sh*",
];

/// 敏感操作判定（Phase 7.2）：命中时返回确认理由（即使 full_auto
/// 也强制确认）；否则 `None`。
fn sensitive_operation(tool_name: &str, command: Option<&str>) -> Option<&'static str> {
    if SENSITIVE_TOOLS.contains(&tool_name) {
        return Some(
            "This tool accesses potentially private data and always requires confirmation.",
        );
    }
    if let Some(command) = command {
        let lowered = command.to_lowercase();
        if SENSITIVE_COMMAND_PATTERNS
            .iter()
            .any(|pattern| fnmatch(&lowered, pattern))
        {
            return Some(
                "This command is potentially destructive and always requires confirmation, \
                 even in full-auto mode.",
            );
        }
    }
    None
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::policy::sandbox_policy::FilesystemPolicy;

    fn engine(mode: PermissionMode) -> Arc<PermissionEngine> {
        PermissionEngine::new(mode, PermissionSettings::default())
    }

    #[test]
    fn full_auto_still_confirms_destructive_shell_command() {
        let engine = engine(PermissionMode::FullAuto);
        let decision =
            engine.evaluate("shell_command", false, Some("/work"), Some("rm -rf /tmp/x"));
        assert!(!decision.allowed, "full_auto 下破坏性命令不得直接放行");
        assert!(decision.requires_confirmation);
        assert!(decision.reason.contains("destructive"));
    }

    #[test]
    fn full_auto_allows_ordinary_command_without_confirmation() {
        let engine = engine(PermissionMode::FullAuto);
        let decision = engine.evaluate("shell_command", false, Some("/work"), Some("echo hi"));
        assert!(decision.allowed);
        assert!(!decision.requires_confirmation);
    }

    #[test]
    fn full_auto_confirms_privacy_tools() {
        let engine = engine(PermissionMode::FullAuto);
        for tool in ["clipboard", "screenshot"] {
            let decision = engine.evaluate(tool, false, None, None);
            assert!(!decision.allowed, "{tool} 应在 full_auto 下仍确认");
            assert!(decision.requires_confirmation);
        }
    }

    #[test]
    fn read_only_privacy_tool_still_confirms_in_default_and_plan() {
        // review 修复回归：只读快路径不得绕过敏感工具门控。若未来某隐私
        // 工具被注册为 is_read_only=true（如"读取剪贴板历史"），default / plan
        // 模式下仍必须确认（full_auto 已由既有测试覆盖，此处验证其余模式）。
        for mode in [PermissionMode::Default, PermissionMode::Plan] {
            let engine = engine(mode);
            for tool in ["clipboard", "screenshot"] {
                let decision = engine.evaluate(tool, true, None, None);
                assert!(
                    !decision.allowed && decision.requires_confirmation,
                    "mode {mode:?} read-only {tool} must confirm: {decision:?}"
                );
                assert!(
                    decision.reason.contains("private data"),
                    "{}",
                    decision.reason
                );
            }
        }
        // 非敏感只读工具不受影响：default 下仍直接放行。
        let engine = engine(PermissionMode::Default);
        let decision = engine.evaluate("read_file", true, None, None);
        assert!(decision.allowed, "{decision:?}");
    }

    #[test]
    fn session_always_allow_cannot_bypass_sensitive_command() {
        let engine = engine(PermissionMode::FullAuto);
        engine.allow_for_session("shell_command");
        let decision = engine.evaluate("shell_command", false, Some("/work"), Some("sudo rm x"));
        assert!(decision.requires_confirmation, "会话放行不得绕过敏感操作");
    }

    #[test]
    fn filesystem_quadrant_denies_write_outside_allow() {
        let engine = PermissionEngine::with_filesystem_policy(
            PermissionMode::FullAuto,
            PermissionSettings::default(),
            FilesystemPolicy {
                allow_write: vec!["/work".into()],
                ..Default::default()
            },
        );
        // 写 allow_write 内→放行
        let ok = engine.evaluate("write_file", false, Some("/work/src/a.rs"), None);
        assert!(ok.allowed);
        // 写名单外→拒绝（四象限白名单模式）
        let denied = engine.evaluate("write_file", false, Some("/etc/passwd"), None);
        assert!(!denied.allowed);
        assert!(denied.reason.contains("sandbox filesystem write policy"));
        // 读象限为空→默认放行
        let read = engine.evaluate("read_file", true, Some("/etc/hosts"), None);
        assert!(read.allowed);
    }

    #[test]
    fn empty_filesystem_policy_is_noop() {
        // 默认构造（空四象限）不影响现有行为
        let engine = engine(PermissionMode::FullAuto);
        assert!(
            engine
                .evaluate("write_file", false, Some("/anywhere/x"), None)
                .allowed
        );
    }

    #[test]
    fn compound_and_recursive_file_access_cannot_bypass_quadrants() {
        let engine = PermissionEngine::with_filesystem_policy(
            PermissionMode::FullAuto,
            PermissionSettings::default(),
            FilesystemPolicy {
                allow_read: vec!["/work".into()],
                allow_write: vec!["/work".into()],
                deny_read: vec!["/work/secrets/*".into()],
                ..Default::default()
            },
        );
        assert!(!engine.file_access_allowed("/work/secrets/key", true));
        assert!(!engine.recursive_read_allowed("/work"));
    }

    #[test]
    fn sensitive_paths_denied_in_every_mode() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::FullAuto,
        ] {
            let engine = engine(mode);
            let decision = engine.evaluate("read_file", true, Some("/home/u/.ssh/id_rsa"), None);
            assert!(!decision.allowed, "mode {mode:?} must deny sensitive path");
            assert!(!decision.requires_confirmation);
            assert!(decision.reason.contains("sensitive credential path"));
        }
    }

    #[test]
    fn sensitive_dir_root_denied_via_trailing_slash_form() {
        // grep/glob 以 ~/.ssh 为 root：目录本身也要被黑名单命中
        let engine = engine(PermissionMode::FullAuto);
        let decision = engine.evaluate("grep", true, Some("/home/u/.ssh"), None);
        assert!(!decision.allowed);
    }

    #[test]
    fn windows_separators_cannot_bypass_sensitive_paths() {
        let engine = engine(PermissionMode::FullAuto);
        let decision =
            engine.evaluate("read_file", true, Some(r"C:\Users\alice\.ssh\id_rsa"), None);
        assert!(!decision.allowed);
        assert!(decision.reason.contains("sensitive credential path"));
    }

    #[test]
    fn exiting_plan_mode_requires_confirmation() {
        let engine = engine(PermissionMode::Plan);
        let decision = engine.evaluate("exit_plan_mode", false, None, None);
        assert!(!decision.allowed);
        assert!(decision.requires_confirmation);
        assert!(decision.reason.contains("requires user confirmation"));
    }

    #[test]
    fn explicit_tool_deny_beats_allow_list() {
        let engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                denied_tools: vec!["shell_command".into()],
                allowed_tools: vec!["shell_command".into()],
                ..Default::default()
            },
        );
        let decision = engine.evaluate("shell_command", false, None, None);
        assert!(!decision.allowed);
        assert!(decision.reason.contains("explicitly denied"));
    }

    #[test]
    fn path_rules_first_match_wins_with_full_allow_deny() {
        let engine = PermissionEngine::new(
            PermissionMode::Plan, // plan 模式下 allow 规则仍应放行（跳过模式门控）
            PermissionSettings {
                path_rules: vec![
                    PathRule {
                        pattern: "*/allowed/*".into(),
                        allow: true,
                    },
                    PathRule {
                        pattern: "*/denied/*".into(),
                        allow: false,
                    },
                ],
                ..Default::default()
            },
        );
        let allowed = engine.evaluate("write_file", false, Some("/w/allowed/a.txt"), None);
        assert!(allowed.allowed, "{}", allowed.reason);
        let denied = engine.evaluate("read_file", true, Some("/w/denied/a.txt"), None);
        assert!(!denied.allowed);
        assert!(denied.reason.contains("deny rule"));
        // 无规则命中的路径回落模式门控（plan 拦截写）
        let fallthrough = engine.evaluate("write_file", false, Some("/w/other/a.txt"), None);
        assert!(!fallthrough.allowed);
        assert!(fallthrough.reason.contains("Plan mode"));
    }

    #[test]
    fn path_deny_rules_override_configured_and_session_tool_allowlists() {
        let engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                allowed_tools: vec!["write_file".into()],
                path_rules: vec![PathRule {
                    pattern: "/workspace/private/*".into(),
                    allow: false,
                }],
                ..Default::default()
            },
        );
        let configured = engine.evaluate(
            "write_file",
            false,
            Some("/workspace/private/secret.txt"),
            None,
        );
        assert!(
            !configured.allowed,
            "configured allowlist bypassed path deny"
        );
        assert!(configured.reason.contains("deny rule"));

        let session_engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                path_rules: vec![PathRule {
                    pattern: "/workspace/private/*".into(),
                    allow: false,
                }],
                ..Default::default()
            },
        );
        session_engine.allow_for_session("write_file");
        let session = session_engine.evaluate(
            "write_file",
            false,
            Some("/workspace/private/secret.txt"),
            None,
        );
        assert!(!session.allowed, "session allowlist bypassed path deny");
        assert!(session.reason.contains("deny rule"));
    }

    #[test]
    fn path_rule_order_dominates_over_candidate_forms() {
        // review 二轮修复回归：前置 deny 规则仅命中尾随 `/` 形态时，
        // 不得被后置 allow 规则（命中首形态）绕过
        let engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                path_rules: vec![
                    PathRule {
                        pattern: "*/x/".into(),
                        allow: false,
                    },
                    PathRule {
                        pattern: "*/x".into(),
                        allow: true,
                    },
                ],
                ..Default::default()
            },
        );
        let decision = engine.evaluate("grep", true, Some("/a/x"), None);
        assert!(!decision.allowed, "前置 deny 规则必须优先生效");
        assert!(decision.reason.contains("deny rule"));
    }

    #[test]
    fn denied_command_glob() {
        let engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                denied_commands: vec!["rm -rf /*".into()],
                ..Default::default()
            },
        );
        let decision = engine.evaluate("shell_command", false, None, Some("rm -rf /tmp"));
        assert!(!decision.allowed);
        assert!(decision.reason.contains("deny pattern"));
    }

    #[test]
    fn denied_command_overrides_all_allow_paths() {
        let configured = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                allowed_tools: vec!["shell_command".into()],
                denied_commands: vec!["rm -rf /*".into()],
                path_rules: vec![PathRule {
                    pattern: "/workspace/*".into(),
                    allow: true,
                }],
                ..Default::default()
            },
        );
        let decision = configured.evaluate(
            "shell_command",
            false,
            Some("/workspace/project"),
            Some("rm -rf /tmp"),
        );
        assert!(!decision.allowed, "path/tool allow bypassed command deny");
        assert!(decision.reason.contains("deny pattern"));

        let session = PermissionEngine::new(
            PermissionMode::Default,
            PermissionSettings {
                denied_commands: vec!["rm -rf /*".into()],
                ..Default::default()
            },
        );
        session.allow_for_session("shell_command");
        let decision = session.evaluate(
            "shell_command",
            false,
            Some("/workspace/project"),
            Some("rm -rf /tmp"),
        );
        assert!(!decision.allowed, "AlwaysAllow bypassed command deny");
        assert!(decision.reason.contains("deny pattern"));
    }

    #[test]
    fn mode_gating_matrix() {
        // full_auto：写放行
        let auto = engine(PermissionMode::FullAuto);
        assert!(auto.evaluate("write_file", false, None, None).allowed);
        // 只读：任何模式放行
        let plan = engine(PermissionMode::Plan);
        assert!(plan.evaluate("read_file", true, None, None).allowed);
        // plan：写拒绝且不询问
        let decision = plan.evaluate("write_file", false, None, None);
        assert!(!decision.allowed && !decision.requires_confirmation);
        // default：写要求确认
        let default = engine(PermissionMode::Default);
        let decision = default.evaluate("write_file", false, None, None);
        assert!(!decision.allowed && decision.requires_confirmation);
    }

    #[test]
    fn default_mode_bash_hint_appended() {
        let engine = engine(PermissionMode::Default);
        let decision = engine.evaluate("shell_command", false, None, Some("npm install left-pad"));
        assert!(decision.requires_confirmation);
        assert!(decision.reason.contains("Package installation"));
    }

    #[test]
    fn session_allowlist_and_mode_switch() {
        let engine = engine(PermissionMode::Default);
        assert!(!engine.evaluate("write_file", false, None, None).allowed);
        engine.allow_for_session("write_file");
        assert!(engine.evaluate("write_file", false, None, None).allowed);
        // 模式切换共享句柄可见
        engine.set_mode(PermissionMode::Plan);
        assert_eq!(engine.mode(), PermissionMode::Plan);
        // 会话放行在 plan 模式下挂起（review 十二轮修复回归）：
        // plan 的只读保证不得被早前的 AlwaysAllow 削弱
        let decision = engine.evaluate("write_file", false, None, None);
        assert!(!decision.allowed && !decision.requires_confirmation);
        assert!(decision.reason.contains("Plan mode"));
        // 退出 plan 后放行集恢复生效
        engine.set_mode(PermissionMode::Default);
        assert!(engine.evaluate("write_file", false, None, None).allowed);
    }

    #[test]
    fn configured_allowlist_still_applies_in_plan_mode() {
        // 配置级 allowed_tools 是宿主显式静态授权：与会话级放行不同，
        // 保持基线序先于模式门控
        let engine = PermissionEngine::new(
            PermissionMode::Plan,
            PermissionSettings {
                allowed_tools: vec!["write_file".into()],
                ..Default::default()
            },
        );
        assert!(engine.evaluate("write_file", false, None, None).allowed);
    }

    #[test]
    fn empty_pattern_rules_are_dropped() {
        let engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                path_rules: vec![PathRule {
                    pattern: "   ".into(),
                    allow: false,
                }],
                ..Default::default()
            },
        );
        assert!(
            engine
                .evaluate("read_file", true, Some("/any/path"), None)
                .allowed
        );
    }

    #[test]
    fn path_allow_rule_cannot_bypass_destructive_command_confirmation() {
        // review 修复回归：PathRule allow 命中 cwd（shell_command 恒以 cwd 作为
        // file_path 求值）时，破坏性命令仍必须二次确认——不得被路径规则短路。
        let engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                path_rules: vec![PathRule {
                    pattern: "*/workspace/*".into(),
                    allow: true,
                }],
                ..Default::default()
            },
        );
        // 破坏性命令：路径 allow 规则命中 → 仍强制确认
        let destructive = engine.evaluate(
            "shell_command",
            false,
            Some("/workspace/project"),
            Some("rm -rf /"),
        );
        assert!(
            !destructive.allowed && destructive.requires_confirmation,
            "path allow rule must not bypass destructive confirmation: {destructive:?}"
        );
        // 安全命令：路径 allow 规则照常放行（不回归基线 allow 语义）
        let safe = engine.evaluate(
            "shell_command",
            false,
            Some("/workspace/project"),
            Some("echo hi"),
        );
        assert!(
            safe.allowed,
            "safe cmd should be allowed via path rule: {safe:?}"
        );
        // 名单外路径：回落模式门控（full_auto 放行）
        let outside = engine.evaluate("shell_command", false, Some("/elsewhere"), Some("echo hi"));
        assert!(
            outside.allowed,
            "full_auto should allow safe cmd: {outside:?}"
        );
    }

    #[test]
    fn path_allow_rule_cannot_bypass_sensitive_confirmation_in_any_mode() {
        // 敏感门控在 PathRule 之前求值：default / plan 下同样不被路径规则绕过
        // （plan 对写操作本就拒绝；此处验证允许路径下敏感命令仍确认）。
        for mode in [PermissionMode::Default, PermissionMode::FullAuto] {
            let engine = PermissionEngine::new(
                mode,
                PermissionSettings {
                    path_rules: vec![PathRule {
                        pattern: "/work/*".into(),
                        allow: true,
                    }],
                    ..Default::default()
                },
            );
            let decision = engine.evaluate(
                "shell_command",
                false,
                Some("/work/project"),
                Some("sudo rm -rf /tmp/x"),
            );
            assert!(
                !decision.allowed && decision.requires_confirmation,
                "mode {mode:?} must confirm sensitive cmd under path allow: {decision:?}"
            );
        }
        // 隐私工具（clipboard/screenshot）同样不受路径规则豁免
        let engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                path_rules: vec![PathRule {
                    pattern: "*/*".into(),
                    allow: true,
                }],
                ..Default::default()
            },
        );
        for tool in ["clipboard", "screenshot"] {
            let decision = engine.evaluate(tool, false, Some("/anywhere"), None);
            assert!(
                !decision.allowed && decision.requires_confirmation,
                "{tool} must confirm under path allow: {decision:?}"
            );
        }
    }

    #[test]
    fn allowed_tools_does_not_bypass_sensitive_operation() {
        // C2 修复：配置级 allowed_tools 放行 shell_command 时，
        // 破坏性命令（rm -rf /）仍须二次确认。
        let engine = PermissionEngine::new(
            PermissionMode::FullAuto,
            PermissionSettings {
                allowed_tools: vec!["shell_command".into()],
                ..Default::default()
            },
        );
        // 破坏性命令 → 强制确认
        let destructive = engine.evaluate("shell_command", false, Some("/work"), Some("rm -rf /"));
        assert!(
            !destructive.allowed,
            "destructive cmd must not be auto-allowed"
        );
        assert!(destructive.requires_confirmation);
        assert!(destructive.reason.contains("destructive"));

        // 安全命令 → allowed_tools 放行
        let safe = engine.evaluate("shell_command", false, Some("/work"), Some("echo hello"));
        assert!(safe.allowed, "safe cmd should be allowed via allowed_tools");
    }
}
