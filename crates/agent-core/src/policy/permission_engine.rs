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
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fnmatch::fnmatch;
use crate::marker::MaybeSendSync;

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

    fn deny(reason: impl Into<String>) -> Self {
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
    async fn confirm(&self, request: &PermissionRequest) -> PermissionReply;
}

/// 权限引擎：规则不可变，模式与会话级放行集内部可变
/// （enter/exit_plan_mode 工具与"总是允许"答复经共享句柄写入）。
pub struct PermissionEngine {
    settings: PermissionSettings,
    mode: RwLock<PermissionMode>,
    /// "总是允许"累积的会话级工具放行集（不持久化）。
    session_allowed: RwLock<HashSet<String>>,
}

impl PermissionEngine {
    pub fn new(mode: PermissionMode, settings: PermissionSettings) -> Arc<Self> {
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
                        // allow 规则命中：跳过模式门控直接放行
                        return PermissionDecision::allow(format!(
                            "Path {path} matches allow rule: {}",
                            rule.pattern
                        ));
                    }
                }
            }
        }

        // 5. 工具显式 allow（配置级 + 会话级"总是允许"）。
        //    敏感路径与 PathRule deny 仍不可被覆盖。
        if self
            .settings
            .allowed_tools
            .iter()
            .any(|allowed| allowed == tool_name)
        {
            return PermissionDecision::allow(format!("{tool_name} is explicitly allowed"));
        }
        // 会话级"总是允许"在 plan 模式下挂起（review 十二轮修复）：早前
        // default 模式下的 AlwaysAllow 答复不得削弱 plan 的只读保证；
        // 放行集本身保留，退出 plan 后恢复生效。配置级 allowed_tools 是
        // 宿主显式静态授权，仍按基线序先于模式门控。
        if self.mode() != PermissionMode::Plan
            && self
                .session_allowed
                .read()
                .expect("session allowlist lock poisoned")
                .contains(tool_name)
        {
            return PermissionDecision::allow(format!("{tool_name} is allowed for this session"));
        }

        // 6. full_auto：全部放行
        if self.mode() == PermissionMode::FullAuto {
            return PermissionDecision::allow("Auto mode allows all tools");
        }

        // 7. 只读工具恒放行
        if is_read_only {
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn engine(mode: PermissionMode) -> Arc<PermissionEngine> {
        PermissionEngine::new(mode, PermissionSettings::default())
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
}
