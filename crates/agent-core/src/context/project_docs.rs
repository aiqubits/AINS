//! 项目指令文件段（Phase 5.3，对齐 OpenHarness `prompts/claudemd.py`；
//! AINS 使用 `AGENTS.md` 而非 `CLAUDE.md`）。
//!
//! 从 cwd 逐级向上发现指令文件，拼接为单个 `# Project Instructions` 段，
//! 每文件截断到字符上限。文件系统访问仅 Native 平台可用；Web 平台无本地
//! 文件系统，返回 `None`（对齐 memory 层双端分工）。

use std::path::Path;

/// 每个项目指令文件注入的最大字符数（对齐基线 `max_chars_per_file = 12000`）。
pub const MAX_CHARS_PER_PROJECT_DOC: usize = 12_000;

/// 项目指令段最多纳入的文件数（就近优先保留）。基线仅做逐文件
/// 截断、无总量上限；AINS 额外加聚合上限，防止深层目录树（每层都有
/// AGENTS.md）把 system prompt 撞胀。
pub const MAX_PROJECT_DOC_FILES: usize = 16;

/// 项目指令段注入的聚合字符上限（各文件已自行截断到
/// [`MAX_CHARS_PER_PROJECT_DOC`]；达阈后不再纳入更远的指令）。
pub const MAX_PROJECT_DOCS_TOTAL_CHARS: usize = 48_000;

/// 单个项目指令文件的最大输入字节数（护栏，超限跳过而非全量读入内存）。
/// 字符预算 [`MAX_CHARS_PER_PROJECT_DOC`]=12000 ≤ 48 KB，1 MiB 为宽松上限，
/// 不影响任何合理体量的指令文件，仅拦截误放/生成的超大文件（`discover`
/// 会上溯至文件系统根，可能命中项目外父目录的超大同名文件）。与感知层
/// `MAX_FILE_BYTES` 同类资源护栏。
pub const MAX_PROJECT_DOC_BYTES: u64 = 1024 * 1024;

/// 截断标记（对齐基线 `...[truncated]...`）。
pub const TRUNCATION_MARKER: &str = "\n...[truncated]...";

/// 每层目录检查的候选相对路径（对齐基线 `<dir>/CLAUDE.md`、
/// `<dir>/.claude/CLAUDE.md` 的 AINS 命名版）。仅 Native 需要（Web 无文件系统）。
#[cfg(not(target_arch = "wasm32"))]
const CANDIDATE_RELATIVES: [&str; 2] = ["AGENTS.md", ".agents/AGENTS.md"];

/// 从 cwd 逐级向上发现指令文件（近者在前，去重）。
///
/// Web 平台无本地文件系统，恒返回空列表。
#[cfg(target_arch = "wasm32")]
pub fn discover_agents_md_files(_cwd: &Path) -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// 从 cwd 逐级向上发现指令文件（Native）：遍历 `[cwd, *cwd.ancestors()]`
/// 到文件系统根，每层顺序检查候选文件，`seen` 去重，顺序为近者在前。
#[cfg(not(target_arch = "wasm32"))]
pub fn discover_agents_md_files(cwd: &Path) -> Vec<std::path::PathBuf> {
    use std::collections::HashSet;

    let start = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut results = Vec::new();
    let mut seen: HashSet<std::path::PathBuf> = HashSet::new();
    for directory in start.ancestors() {
        for relative in CANDIDATE_RELATIVES {
            let candidate = directory.join(relative);
            if candidate.is_file() && seen.insert(candidate.clone()) {
                results.push(candidate);
            }
        }
    }
    results
}

/// 加载并渲染 `# Project Instructions` 段（无文件返回 `None`）。
///
/// Web 平台恒返回 `None`。
#[cfg(target_arch = "wasm32")]
pub fn load_project_instructions(_cwd: &Path) -> Option<String> {
    None
}

/// 加载并渲染 `# Project Instructions` 段（Native）：每文件截断到
/// [`MAX_CHARS_PER_PROJECT_DOC`]，格式为 `## <path>` + md 代码块。
#[cfg(not(target_arch = "wasm32"))]
pub fn load_project_instructions(cwd: &Path) -> Option<String> {
    let files = discover_agents_md_files(cwd);
    if files.is_empty() {
        return None;
    }
    let mut sections: Vec<String> = Vec::new();
    let mut total_chars = 0usize;
    // 就近优先：discover 返回近者在前；超出文件数 / 聚合字符预算
    // 即停止纳入更远的指令（至少保留最近一个段）。
    for path in files.iter().take(MAX_PROJECT_DOC_FILES) {
        // 跳过异常大的指令文件（远超字符预算即视为误放/生成文件），避免对
        // 超大文件做全量读入内存（metadata 探测大小失败则交由下方读取处理）。
        if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_PROJECT_DOC_BYTES) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let section = render_doc_section(&path.display().to_string(), &raw);
        let section_chars = section.chars().count();
        if !sections.is_empty()
            && total_chars.saturating_add(section_chars) > MAX_PROJECT_DOCS_TOTAL_CHARS
        {
            break;
        }
        total_chars = total_chars.saturating_add(section_chars);
        sections.push(section);
    }
    if sections.is_empty() {
        return None;
    }
    Some(format!(
        "# Project Instructions\n\n{}",
        sections.join("\n\n")
    ))
}

/// 渲染单个文件段：`## <path>` + md 代码块（超限截断 + 标记）。
/// 抽出为纯函数以便双端单测（不依赖真实文件系统）。
pub fn render_doc_section(path_label: &str, content: &str) -> String {
    let body = truncate_doc(content);
    format!("## {path_label}\n```md\n{}\n```", body.trim())
}

/// 按字符截断文件内容（超限追加截断标记，对齐基线语义）。
pub fn truncate_doc(content: &str) -> String {
    if content.chars().count() <= MAX_CHARS_PER_PROJECT_DOC {
        return content.to_string();
    }
    let truncated: String = content.chars().take(MAX_CHARS_PER_PROJECT_DOC).collect();
    format!("{truncated}{TRUNCATION_MARKER}")
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn truncate_doc_keeps_short_content_verbatim() {
        assert_eq!(truncate_doc("short"), "short");
    }

    #[test]
    fn truncate_doc_caps_long_content_with_marker() {
        let long = "x".repeat(MAX_CHARS_PER_PROJECT_DOC + 50);
        let out = truncate_doc(&long);
        assert!(out.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            out.chars().count(),
            MAX_CHARS_PER_PROJECT_DOC + TRUNCATION_MARKER.chars().count()
        );
    }

    #[test]
    fn render_doc_section_wraps_in_labeled_code_fence() {
        let section = render_doc_section("/proj/AGENTS.md", "  build with cargo  ");
        assert_eq!(section, "## /proj/AGENTS.md\n```md\nbuild with cargo\n```");
    }

    #[test]
    fn discover_and_load_walk_upward_and_concatenate() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "nested rules").unwrap();

        let files = discover_agents_md_files(&nested);
        // 近者（nested）在前，根在后
        assert!(files.len() >= 2);
        assert!(
            files[0].ends_with("a/b/AGENTS.md"),
            "nearest file should be first: {files:?}"
        );

        let prompt = load_project_instructions(&nested).unwrap();
        assert!(prompt.starts_with("# Project Instructions"));
        assert!(prompt.contains("nested rules"));
        assert!(prompt.contains("root rules"));
        // nested 段应在 root 段之前
        let nested_at = prompt.find("nested rules").unwrap();
        let root_at = prompt.find("root rules").unwrap();
        assert!(nested_at < root_at);
    }

    #[test]
    fn load_returns_none_without_agents_files() {
        let root = tempfile::tempdir().unwrap();
        // canonicalize 后逐级向上仍可能命中真实仓库根的 AGENTS.md，
        // 因此仅断言不会 panic 且类型正确。
        let _ = load_project_instructions(root.path());
    }

    #[test]
    fn load_caps_file_count_on_deep_tree() {
        // 构造超过 MAX_PROJECT_DOC_FILES 层、每层一个 AGENTS.md 的深层目录树，
        // 验证注入段数被聚合上限截断，且就近（最深）文件保留、最远被舍弃。
        let root = tempfile::tempdir().unwrap();
        let levels = MAX_PROJECT_DOC_FILES + 4;
        let mut dir = root.path().to_path_buf();
        for depth in 0..levels {
            dir = dir.join(format!("lvl{depth}"));
            std::fs::create_dir_all(&dir).unwrap();
            // 每层内容带唯一标记 marker_{depth}（不含 Markdown 标题，避免污染计数）。
            std::fs::write(dir.join("AGENTS.md"), format!("marker_{depth} rules")).unwrap();
        }
        // dir 此时为最深层（depth = levels-1）
        let prompt = load_project_instructions(&dir).unwrap();
        // 段数上限：统计 "## " 头部不超过 MAX_PROJECT_DOC_FILES
        let section_count = prompt.matches("## ").count();
        assert!(
            section_count <= MAX_PROJECT_DOC_FILES,
            "sections {section_count} must be capped at {MAX_PROJECT_DOC_FILES}"
        );
        // 就近优先：最深层保留，最远层（marker_0）被舍弃
        assert!(prompt.contains(&format!("marker_{} rules", levels - 1)));
        assert!(!prompt.contains("marker_0 rules"));
    }

    #[test]
    fn load_skips_oversized_doc_but_keeps_normal_sibling() {
        // 回归（超越基线）：超大指令文件应跳过而非全量读入内存；同目录树中
        // 正常体量的指令文件仍纳入。
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("child");
        std::fs::create_dir_all(&nested).unwrap();
        // 父层：超大 AGENTS.md（> MAX_PROJECT_DOC_BYTES）
        std::fs::write(
            root.path().join("AGENTS.md"),
            "x".repeat(MAX_PROJECT_DOC_BYTES as usize + 1),
        )
        .unwrap();
        // 近层：正常大小
        std::fs::write(nested.join("AGENTS.md"), "nested normal rules").unwrap();

        let prompt = load_project_instructions(&nested).unwrap();
        // 近层正常文件纳入
        assert!(prompt.contains("nested normal rules"));
        // 超大父层文件被跳过：其内容（长串 x）不应注入（对 ancestors 命中鲁棒）
        assert!(
            !prompt.contains(&"x".repeat(100)),
            "oversized doc content must not be injected"
        );
    }
}
