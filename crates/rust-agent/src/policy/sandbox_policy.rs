//! 可移植 Sandbox 策略模型（Phase 7.1 Layer 1，纯 Rust 双 target）。
//!
//! 对齐 Harness `config/settings.py` 的 `SandboxNetworkSettings` /
//! `SandboxFilesystemSettings`：网络域白/黑名单 + 文件系统读写四象限。
//!
//! 本层是**平台无关**的策略判定，对 web / mobile / desktop 一致生效：
//! - web_fetch 出网前按 [`NetworkPolicy`] 校验域名；
//! - 文件工具按 [`FilesystemPolicy`] 校验读/写路径象限。
//!
//! Desktop 原生另有 Layer 2（`sandbox_*.rs` 平台运行时）把本策略下推进
//! OS 级隔离（bwrap `--ro-bind`/`--bind`/网络开关）。

use serde::{Deserialize, Serialize};

use std::path::Path;

use crate::fnmatch::fnmatch;

/// 网络域规则（对齐 AINS_PLAN `DomainRule`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainRule {
    pub domain: String,
    /// `true` = allow，`false` = deny。
    pub allow: bool,
}

/// 网络访问策略：域名白/黑名单（对齐基线 `SandboxNetworkSettings`）。
///
/// 判定序：deny 优先（黑名单命中即拒）→ 白名单非空时必须命中方可放行
/// （白名单模式）→ 白名单为空则默认放行（仅黑名单模式）。域名匹配为
/// 精确或子域（规则 `example.com` 命中 `example.com` 与 `*.example.com`；
/// 规则可写 `*.example.com` 等价形式）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPolicy {
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub denied_domains: Vec<String>,
}

impl NetworkPolicy {
    /// 主机名是否被本策略放行。空策略（无 allow/deny）默认放行，保持
    /// 未配置时的既有行为。
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_lowercase();
        if self
            .denied_domains
            .iter()
            .any(|rule| domain_matches(&host, rule))
        {
            return false;
        }
        if self.allowed_domains.is_empty() {
            return true;
        }
        self.allowed_domains
            .iter()
            .any(|rule| domain_matches(&host, rule))
    }

    /// 校验并给出可读拒绝原因（供工具层生成 is_error 文案）。
    pub fn check_host(&self, host: &str) -> Result<(), String> {
        if self.allows_host(host) {
            Ok(())
        } else {
            Err(format!(
                "host {host} is blocked by the sandbox network policy"
            ))
        }
    }

    /// 是否整体封锁网络（deny 名单含通配 `*`）。用于沙箱化 shell 的粗粒度
    /// 网络开关：`true` 时 bwrap 不 `--share-net`（全断）。
    pub fn blocks_all(&self) -> bool {
        self.denied_domains.iter().any(|rule| rule.trim() == "*")
    }
}

/// 域名规则匹配：`*` 通配全部；否则规则去除可选前导 `*.` 后，命中精确
/// 域名或其子域。
fn domain_matches(host: &str, rule: &str) -> bool {
    let rule = rule.trim().trim_end_matches('.').to_lowercase();
    if rule == "*" {
        return true;
    }
    let rule = rule.strip_prefix("*.").unwrap_or(&rule);
    if rule.is_empty() {
        return false;
    }
    host == rule || host.ends_with(&format!(".{rule}"))
}

/// 文件系统读写四象限策略（对齐基线 `SandboxFilesystemSettings`）。
///
/// 每个象限为 glob 列表。判定：deny 优先 → allow 非空时必须命中
/// （白名单模式）→ allow 为空则默认放行。空策略默认全放行（保持未配置
/// 时的既有行为；限制性策略由宿主显式装配）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemPolicy {
    #[serde(default)]
    pub allow_read: Vec<String>,
    #[serde(default)]
    pub deny_read: Vec<String>,
    #[serde(default)]
    pub allow_write: Vec<String>,
    #[serde(default)]
    pub deny_write: Vec<String>,
}

impl FilesystemPolicy {
    pub fn can_read(&self, path: &str) -> bool {
        quadrant_allows(path, &self.allow_read, &self.deny_read)
    }

    pub fn can_write(&self, path: &str) -> bool {
        quadrant_allows(path, &self.allow_write, &self.deny_write)
    }
}

/// 单象限判定：deny 优先，allow 非空须命中，allow 为空默认放行。
fn quadrant_allows(path: &str, allow: &[String], deny: &[String]) -> bool {
    if deny.iter().any(|pattern| fs_path_matches(path, pattern)) {
        return false;
    }
    if allow.is_empty() {
        return true;
    }
    allow.iter().any(|pattern| fs_path_matches(path, pattern))
}

/// 路径 glob 匹配：统一分隔符为 `/`（防 Windows 反斜杠旁路），支持
/// 目录前缀（`allow=/work` 命中 `/work` 及其子路径）。
///
/// 根目录规则 `"/"`（含 `"//"` 等全斜杠形态）匹配**一切路径**：`deny_read:
/// ["/"]` 表示拒绝读取全部（fail-closed），`allow_read: ["/"]` 表示放行全部。
/// 历史上 `trim_end_matches('/')` 会把 `"/"` 变成空串而静默丢弃——deny 根
/// 规则失效会退化为全放行（fail-open），属安全语义错误。
fn fs_path_matches(path: &str, pattern: &str) -> bool {
    let path = path.replace('\\', "/");
    let pattern = pattern.replace('\\', "/");
    #[cfg(target_os = "windows")]
    let (path, pattern) = {
        // Windows filesystem lookups are case-insensitive; policy matching
        // must not let casing bypass an allow/deny glob.
        (path.to_ascii_lowercase(), pattern.to_ascii_lowercase())
    };
    if pattern.chars().all(|c| c == '/') {
        // 全斜杠（"/" 或 "//" 等）→ 根目录规则，命中一切路径。
        return !pattern.is_empty();
    }
    let pattern = pattern.trim_end_matches('/');
    if pattern.is_empty() {
        return false;
    }
    // 目录前缀：规则本身、其目录形态或其下任意子路径均命中。
    // 注意 `path == pattern` 仅在 pattern 为纯前缀字面量（不含 `*`/`?`/`[`
    // 等 fnmatch 元字符）时提供精确匹配语义；其他情况由 fnmatch 分支覆盖。
    fnmatch(&path, pattern)
        || path == pattern
        || path.starts_with(&format!("{pattern}/"))
        || fnmatch(&path, &format!("{pattern}/*"))
}

/// 完整 Sandbox 策略（网络 + 文件系统），随 `SandboxPolicy` 注入
/// 权限引擎（文件象限）与平台 Sandbox（下推 OS 隔离）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub filesystem: FilesystemPolicy,
}

impl SandboxPolicy {
    /// Whether a shell backend may make `cwd` its read/write working tree
    /// without violating the filesystem quadrants.  Native shell backends need
    /// both permissions for `chdir` and for their writable working-tree bind;
    /// when either allowlist excludes the directory, running the shell would
    /// silently widen a restrictive policy.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) fn shell_cwd_is_allowed(&self, cwd: &Path) -> bool {
        let cwd = cwd.to_string_lossy();
        self.filesystem.can_read(&cwd) && self.filesystem.can_write(&cwd)
    }

    /// Whether this policy asks a shell backend to enforce any network or
    /// filesystem restriction.  A backend that cannot do so must refuse shell
    /// execution instead of silently treating the policy as advisory.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn requires_shell_policy_enforcement(&self) -> bool {
        !self.network.allowed_domains.is_empty()
            || !self.network.denied_domains.is_empty()
            || !self.filesystem.allow_read.is_empty()
            || !self.filesystem.deny_read.is_empty()
            || !self.filesystem.allow_write.is_empty()
            || !self.filesystem.deny_write.is_empty()
    }

    /// Whether every restriction that the macOS shell backend needs to enforce
    /// can be represented by its SBPL profile.  `sandbox-exec` has no domain
    /// filtering and SBPL `subpath` cannot express the glob rules accepted by
    /// Layer 1.  Refuse shell rather than silently widening a deny rule or
    /// silently dropping an allow rule from the profile.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn macos_shell_policy_is_enforceable(&self) -> bool {
        if !self.network.allowed_domains.is_empty()
            || (!self.network.denied_domains.is_empty() && !self.network.blocks_all())
        {
            return false;
        }
        self.filesystem
            .allow_read
            .iter()
            .chain(&self.filesystem.allow_write)
            .chain(&self.filesystem.deny_read)
            .chain(&self.filesystem.deny_write)
            .all(|entry| sbpl_bindable(entry).is_some())
    }
}

/// 运行 sh 与常用工具所需的最小 macOS 只读系统目录（dyld 缓存、
/// 系统库与二进制）。四象限可限制其他读，但这些必须可读否则
/// 连 `/bin/sh` 都无法启动。
// 仅 macOS 适配与（Linux）单测使用；非 macOS 的 lib 构建中为死代码。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_SYSTEM_READ_SUBPATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/var/db/dyld",
    "/dev",
];

/// macOS shell 进行名称解析所需的最小配置文件。严格读白名单下不得将
/// 整个 `/etc`（实际常解析至 `/private/etc`）加入 profile，否则任意机器
/// 配置会绕过 allow_read；同时列出两种规范路径以覆盖该符号链接布局。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_SYSTEM_READ_LITERALS: &[&str] = &[
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/nsswitch.conf",
    "/private/etc/hosts",
    "/private/etc/resolv.conf",
    "/private/etc/nsswitch.conf",
];

/// SBPL 字符串字面量转义（Scheme 风格：`\` 与 `"` 需转义）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn sbpl_escape(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 可进入 SBPL `subpath` 字面量的路径条目（与 Linux `bindable_path` 同口径）：
/// 仅绝对、不含 glob 元字符的条目可表达；glob / 相对条目跳过——SBPL 的
/// `subpath` 是字面量前缀匹配，glob 规则无法表达，静默按字面量处理会使其
/// 失效（限制性规则失效 = 放宽限制，fail-open 方向）。glob 规则由 Layer 1
/// 权限引擎（四象限 fnmatch）精确执行。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn sbpl_bindable(entry: &str) -> Option<String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() || trimmed.contains(['*', '?', '[']) {
        return None;
    }
    let path = std::path::Path::new(trimmed);
    if path.is_absolute() {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// 由 [`SandboxPolicy`] + 当前 cwd 生成 macOS `sandbox-exec` 的 SBPL profile
/// （纯函数，平台无关，可在 Linux 单测）。
///
/// 语义镜像四象限：`(deny default)` 打底 → 按策略 allow 读/写子树
/// → deny_* 作为例外覆盖（SBPL 后匹配优先）。写象限恒含 cwd；读象限
/// 为空时放开全读（与 [`FilesystemPolicy::can_read`] 空策略=全放行一致），
/// 否则仅列出项 + 系统只读目录。网络：非全断则放行出站（粗粒度，
/// 与 Linux bwrap `--share-net` 一致；域名级由 Layer 1 web_fetch 精确执行）。
// 仅 macOS 适配与（Linux）单测使用；非 macOS 的 lib 构建中为死代码。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_sbpl_profile(policy: &SandboxPolicy, cwd: &Path) -> String {
    let cwd = cwd.to_string_lossy();
    let mut out = String::new();
    out.push_str("(version 1)\n(deny default)\n");
    // 进程与运行 sh/工具所需基础能力。
    out.push_str("(allow process-fork)\n(allow process-exec)\n");
    out.push_str("(allow sysctl-read)\n(allow mach-lookup)\n");
    out.push_str("(allow signal (target self))\n");

    // 读象限。
    if policy.filesystem.allow_read.is_empty() {
        out.push_str("(allow file-read*)\n");
    } else {
        out.push_str("(allow file-read*\n");
        for entry in MACOS_SYSTEM_READ_SUBPATHS {
            out.push_str(&format!("  (subpath \"{}\")\n", sbpl_escape(entry)));
        }
        for entry in MACOS_SYSTEM_READ_LITERALS {
            out.push_str(&format!("  (literal \"{}\")\n", sbpl_escape(entry)));
        }
        for entry in &policy.filesystem.allow_read {
            if let Some(path) = sbpl_bindable(entry) {
                out.push_str(&format!("  (subpath \"{}\")\n", sbpl_escape(&path)));
            }
        }
        out.push_str(&format!("  (subpath \"{}\"))\n", sbpl_escape(&cwd)));
    }
    for entry in &policy.filesystem.deny_read {
        if let Some(path) = sbpl_bindable(entry) {
            out.push_str(&format!(
                "(deny file-read* (subpath \"{}\"))\n",
                sbpl_escape(&path)
            ));
        }
    }

    // 写象限：cwd + allow_write + 标准流节点。不要无条件放行系统临时
    // 目录：在严格白名单下那会扩大 shell 的实际写入面；运行时 TMPDIR
    // 由 macOS 后端指向 cwd。
    out.push_str(&format!(
        "(allow file-write*\n  (subpath \"{}\")\n",
        sbpl_escape(&cwd)
    ));
    for entry in &policy.filesystem.allow_write {
        if let Some(path) = sbpl_bindable(entry) {
            out.push_str(&format!("  (subpath \"{}\")\n", sbpl_escape(&path)));
        }
    }
    out.push_str(
        "  (literal \"/dev/null\")\n  (literal \"/dev/stdout\")\n  (literal \"/dev/stderr\"))\n",
    );
    for entry in &policy.filesystem.deny_write {
        if let Some(path) = sbpl_bindable(entry) {
            out.push_str(&format!(
                "(deny file-write* (subpath \"{}\"))\n",
                sbpl_escape(&path)
            ));
        }
    }

    // 网络：粗粒度开关（非全断则放行出站）。
    if !policy.network.blocks_all() {
        out.push_str("(allow network*)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_network_policy_allows_all() {
        let policy = NetworkPolicy::default();
        assert!(policy.allows_host("example.com"));
        assert!(policy.allows_host("anything.internal"));
    }

    #[test]
    fn network_deny_takes_precedence_over_allow() {
        let policy = NetworkPolicy {
            allowed_domains: vec!["example.com".into()],
            denied_domains: vec!["evil.example.com".into()],
        };
        assert!(policy.allows_host("example.com"));
        assert!(policy.allows_host("api.example.com"));
        // deny 子域优先于 allow 父域
        assert!(!policy.allows_host("evil.example.com"));
        // 白名单模式：名单外域名默认拒绝
        assert!(!policy.allows_host("other.com"));
    }

    #[test]
    fn network_denylist_only_mode_defaults_allow() {
        let policy = NetworkPolicy {
            allowed_domains: vec![],
            denied_domains: vec!["blocked.com".into()],
        };
        assert!(policy.allows_host("anything.com"));
        assert!(!policy.allows_host("blocked.com"));
        assert!(!policy.allows_host("sub.blocked.com"));
        assert!(!policy.blocks_all());
    }

    #[test]
    fn network_deny_star_blocks_all_hosts() {
        let policy = NetworkPolicy {
            allowed_domains: vec!["example.com".into()],
            denied_domains: vec!["*".into()],
        };
        // deny `*` 优先于任何 allow，封锁全部主机
        assert!(!policy.allows_host("example.com"));
        assert!(!policy.allows_host("anything.com"));
        assert!(policy.blocks_all());
    }

    #[test]
    fn network_wildcard_and_subdomain_matching() {
        let policy = NetworkPolicy {
            allowed_domains: vec!["*.example.com".into()],
            denied_domains: vec![],
        };
        // `*.` 前缀等价于"域名及其子域"
        assert!(policy.allows_host("example.com"));
        assert!(policy.allows_host("a.b.example.com"));
        assert!(!policy.allows_host("notexample.com"));
        // 尾点与大小写归一
        assert!(policy.allows_host("API.EXAMPLE.COM."));
    }

    #[test]
    fn empty_filesystem_policy_allows_all() {
        let policy = FilesystemPolicy::default();
        assert!(policy.can_read("/anywhere/x"));
        assert!(policy.can_write("/anywhere/y"));
    }

    #[test]
    fn filesystem_deny_precedence_and_allowlist_mode() {
        let policy = FilesystemPolicy {
            allow_write: vec!["/work".into()],
            deny_write: vec!["/work/.git/*".into()],
            ..Default::default()
        };
        // allow 前缀命中目录及子路径
        assert!(policy.can_write("/work"));
        assert!(policy.can_write("/work/src/main.rs"));
        // deny 优先
        assert!(!policy.can_write("/work/.git/config"));
        // 白名单模式：名单外拒绝
        assert!(!policy.can_write("/etc/passwd"));
        // 读象限为空 → 默认全放行
        assert!(policy.can_read("/etc/hosts"));
    }

    #[test]
    fn filesystem_windows_separator_cannot_bypass() {
        let policy = FilesystemPolicy {
            deny_read: vec!["/secret/*".into()],
            ..Default::default()
        };
        assert!(!policy.can_read(r"\secret\key"));
        #[cfg(target_os = "windows")]
        assert!(!policy.can_read(r"\SECRET\KEY"));
    }

    #[test]
    fn root_deny_pattern_is_fail_closed_not_dropped() {
        // 根规则 "/" 必须命中一切路径：deny_read=["/"] 拒绝读取全部
        // （回归：历史实现 trim 后为空串被静默丢弃 → 退化为全放行）。
        let policy = FilesystemPolicy {
            deny_read: vec!["/".into()],
            ..Default::default()
        };
        assert!(!policy.can_read("/etc/hosts"));
        assert!(!policy.can_read("/work/x.rs"));
        assert!(!policy.can_read("relative.txt"));
        // 全斜杠变体同样命中
        let slashes = FilesystemPolicy {
            deny_read: vec!["//".into()],
            ..Default::default()
        };
        assert!(!slashes.can_read("/any/path"));
    }

    #[test]
    fn root_allow_pattern_permits_everything() {
        let policy = FilesystemPolicy {
            allow_read: vec!["/".into()],
            deny_read: vec![],
            ..Default::default()
        };
        assert!(policy.can_read("/anything/at/all"));
        // deny 仍优先于根 allow
        let with_deny = FilesystemPolicy {
            allow_read: vec!["/".into()],
            deny_read: vec!["/secret/*".into()],
            ..Default::default()
        };
        assert!(with_deny.can_read("/public/x"));
        assert!(!with_deny.can_read("/secret/key"));
    }

    #[test]
    fn shell_policy_capability_checks_fail_closed_for_unsupported_backends() {
        assert!(!SandboxPolicy::default().requires_shell_policy_enforcement());

        let workspace_only = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec!["/work".into()],
                allow_write: vec!["/work".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(workspace_only.requires_shell_policy_enforcement());
        assert!(workspace_only.macos_shell_policy_is_enforceable());

        let glob_deny = SandboxPolicy {
            filesystem: FilesystemPolicy {
                deny_write: vec!["/work/.git/*".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!glob_deny.macos_shell_policy_is_enforceable());

        let glob_allow = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_write: vec!["/work/*".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!glob_allow.macos_shell_policy_is_enforceable());

        let domain_allow = SandboxPolicy {
            network: NetworkPolicy {
                allowed_domains: vec!["api.example.com".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!domain_allow.macos_shell_policy_is_enforceable());
    }

    #[test]
    fn shell_cwd_must_be_allowed_by_both_filesystem_quadrants() {
        let workspace_only = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec!["/work/project".into()],
                allow_write: vec!["/work/project".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(workspace_only.shell_cwd_is_allowed(Path::new("/work/project")));

        let read_elsewhere = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec!["/data".into()],
                allow_write: vec!["/work/project".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!read_elsewhere.shell_cwd_is_allowed(Path::new("/work/project")));

        let write_elsewhere = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec!["/work/project".into()],
                allow_write: vec!["/data".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!write_elsewhere.shell_cwd_is_allowed(Path::new("/work/project")));

        let denied = SandboxPolicy {
            filesystem: FilesystemPolicy {
                deny_write: vec!["/work/project".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!denied.shell_cwd_is_allowed(Path::new("/work/project")));
    }

    #[test]
    fn sbpl_profile_denies_by_default_and_confines_writes_to_cwd() {
        let profile = macos_sbpl_profile(&SandboxPolicy::default(), Path::new("/work/project"));
        // 默认拒绝打底
        assert!(profile.contains("(deny default)"));
        // 空读象限 → 全读放行
        assert!(profile.contains("(allow file-read*)"));
        // 写限定 cwd
        assert!(profile.contains("(allow file-write*\n  (subpath \"/work/project\")"));
        assert!(!profile.contains("/private/tmp"));
        assert!(!profile.contains("/private/var/folders"));
        // 空网络策略（非全断）→ 放行出站
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn sbpl_profile_skips_glob_and_relative_entries() {
        // T8 回归：SBPL `subpath` 不支持 glob——glob/相对条目必须被跳过，
        // 不得按字面量路径写入 profile（限制性 glob 规则失效 = 放宽限制）。
        let policy = SandboxPolicy {
            filesystem: FilesystemPolicy {
                allow_read: vec!["/data/in".into(), "/data/*".into()],
                allow_write: vec!["/data/out".into(), "relative/skip".into()],
                deny_read: vec!["/secret/*".into()],
                deny_write: vec!["/data/out/.git/*".into()],
            },
            ..Default::default()
        };
        let profile = macos_sbpl_profile(&policy, Path::new("/work"));
        // 可绑定条目照常写入
        assert!(profile.contains("(subpath \"/data/in\")"));
        assert!(profile.contains("(subpath \"/data/out\")"));
        // glob / 相对条目被跳过（不按字面量出现）
        assert!(!profile.contains("/data/*"));
        assert!(!profile.contains("relative/skip"));
        assert!(!profile.contains("/secret/*"));
        assert!(!profile.contains("/data/out/.git/*"));
    }

    #[test]
    fn sbpl_profile_restricts_reads_and_blocks_network_when_configured() {
        let policy = SandboxPolicy {
            network: NetworkPolicy {
                allowed_domains: vec![],
                denied_domains: vec!["*".into()],
            },
            filesystem: FilesystemPolicy {
                allow_read: vec!["/data/in".into()],
                allow_write: vec!["/data/out".into()],
                deny_write: vec!["/data/out/.git".into()],
                ..Default::default()
            },
        };
        let profile = macos_sbpl_profile(&policy, Path::new("/work"));
        // 非空读象限 → 仅列出项 + 系统目录（不得全放行）
        assert!(!profile.contains("(allow file-read*)\n"));
        assert!(profile.contains("(subpath \"/data/in\")"));
        assert!(profile.contains("(subpath \"/usr\")"));
        assert!(!profile.contains("(subpath \"/etc\")"));
        assert!(profile.contains("(literal \"/etc/resolv.conf\")"));
        // 写白名单 + deny 例外
        assert!(profile.contains("(subpath \"/data/out\")"));
        assert!(profile.contains("(deny file-write* (subpath \"/data/out/.git\"))"));
        // 全断网络 → 不放行出站
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn sbpl_escape_handles_quotes_and_backslashes() {
        let policy = FilesystemPolicy {
            allow_write: vec![r#"/w/a"b\c"#.into()],
            ..Default::default()
        };
        let profile = macos_sbpl_profile(
            &SandboxPolicy {
                filesystem: policy,
                ..Default::default()
            },
            Path::new("/w"),
        );
        // 引号与反斜杠均被转义，不破坏 SBPL 字面量
        assert!(profile.contains(r#"(subpath "/w/a\"b\\c")"#));
    }
}
