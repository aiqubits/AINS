//! Environment 段探测（Phase 5.3，对齐 OpenHarness `prompts/environment.py`
//! 与 `_format_environment_section`）。
//!
//! 纯数据结构 + 渲染，不产生子进程、不读配置文件（对齐 AINS_PLAN 附录 C
//! 客户端配置边界）：OS/Architecture 取编译期常量、cwd 来自 Kernel 配置、
//! Date 由毫秒时钟格式化。Shell / Git 分支为宿主可选注入项（Native 平台
//! 若已在 sandbox 边界内探测，可通过 builder 注入），默认省略。

use std::path::Path;

use crate::memory::format_iso_utc;
use crate::platform::Platform;

/// 运行环境快照（对齐基线 `EnvironmentInfo` 的提示词可见子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentInfo {
    /// 操作系统名（编译期 `std::env::consts::OS`，wasm 端为 "unknown"）。
    pub os: String,
    /// CPU 架构（编译期 `std::env::consts::ARCH`，wasm 端为 "wasm32"）。
    pub arch: String,
    /// 运行平台（Web / Desktop / Mobile）。
    pub platform: Platform,
    /// 工作目录（来自 Kernel 配置的 cwd）。
    pub cwd: String,
    /// UTC 日期 `YYYY-MM-DD`（由毫秒时钟格式化）。
    pub date: String,
    /// 用户 shell（宿主可选注入；默认省略）。
    pub shell: Option<String>,
    /// Git 分支（宿主可选注入；`Some` 表示在 git 仓库内）。
    pub git_branch: Option<String>,
}

impl EnvironmentInfo {
    /// 探测环境（纯函数：编译期常量 + 入参 cwd + 毫秒时钟）。
    pub fn detect(cwd: &Path, now_ms: i64) -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            platform: Platform::current(),
            cwd: cwd.display().to_string(),
            // 取 ISO-8601 的日期部分（前 10 字符 `YYYY-MM-DD`）。
            date: format_iso_utc(now_ms).chars().take(10).collect(),
            shell: None,
            git_branch: None,
        }
    }

    /// 注入 shell 名（Native 宿主在 sandbox 边界内探测后可选调用）。
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    /// 注入 git 分支（`None` 表示不在 git 仓库内）。
    pub fn with_git_branch(mut self, branch: Option<String>) -> Self {
        self.git_branch = branch;
        self
    }

    /// 渲染 `# Environment` 段（对齐基线 `_format_environment_section` 的逐行格式）。
    pub fn render(&self) -> String {
        let mut lines = vec![
            "# Environment".to_string(),
            format!("- OS: {}", self.os),
            format!("- Architecture: {}", self.arch),
            format!("- Platform: {}", platform_label(self.platform)),
            format!("- Working directory: {}", self.cwd),
            format!("- Date: {}", self.date),
        ];
        if let Some(shell) = &self.shell {
            lines.push(format!("- Shell: {shell}"));
        }
        if let Some(branch) = &self.git_branch {
            lines.push(format!("- Git: yes (branch: {branch})"));
        }
        lines.join("\n")
    }
}

fn platform_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Web => "Web",
        Platform::Desktop => "Desktop",
        Platform::Mobile => "Mobile",
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_fills_compile_time_constants_and_date() {
        // 2021-01-01T00:00:00Z = 1_609_459_200_000 ms
        let env = EnvironmentInfo::detect(&PathBuf::from("/work/project"), 1_609_459_200_000);
        assert_eq!(env.os, std::env::consts::OS);
        assert_eq!(env.arch, std::env::consts::ARCH);
        assert_eq!(env.cwd, "/work/project");
        assert_eq!(env.date, "2021-01-01");
        assert!(env.shell.is_none());
        assert!(env.git_branch.is_none());
    }

    #[test]
    fn render_lists_core_lines_and_omits_absent_optionals() {
        let env = EnvironmentInfo::detect(&PathBuf::from("/w"), 1_609_459_200_000);
        let rendered = env.render();
        assert!(rendered.starts_with("# Environment"));
        assert!(rendered.contains("- OS: "));
        assert!(rendered.contains("- Architecture: "));
        assert!(rendered.contains("- Working directory: /w"));
        assert!(rendered.contains("- Date: 2021-01-01"));
        assert!(!rendered.contains("- Shell:"));
        assert!(!rendered.contains("- Git:"));
    }

    #[test]
    fn render_includes_injected_shell_and_git() {
        let env = EnvironmentInfo::detect(&PathBuf::from("/w"), 1_609_459_200_000)
            .with_shell("zsh")
            .with_git_branch(Some("main".to_string()));
        let rendered = env.render();
        assert!(rendered.contains("- Shell: zsh"));
        assert!(rendered.contains("- Git: yes (branch: main)"));
    }
}
