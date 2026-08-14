//! 工具输出 inline/preview 字符预算（对齐 Harness `services/tool_outputs.py`
//! + `engine/query.py::_offload_tool_output_if_needed`）。
//!
//! 超长工具输出外置存储（ArtifactSink）并在 tool_result 中留引用 + 内联预览，
//! 避免单次工具输出击穿上下文窗口；旧工具结果是 microcompact 的优先清理对象
//! （context/compact 随 Phase 5 落地，此处先提供判定函数）。
//!
//! 字符计数口径与基线一致：Unicode 标量（Python `len(str)` / 切片语义），
//! 非字节数。环境变量覆盖仅 Native 端生效（WASM 无进程环境，取默认值）。

use crate::marker::MaybeSendSync;

pub const DEFAULT_TOOL_OUTPUT_INLINE_CHARS: usize = 16_000;
pub const DEFAULT_TOOL_OUTPUT_PREVIEW_CHARS: usize = 3_000;
pub const DEFAULT_MICROCOMPACT_TOOL_RESULT_CHARS: usize = 4_000;

/// inline 预算：`AINS_TOOL_OUTPUT_INLINE_CHARS`（下限 256）。
pub fn tool_output_inline_chars() -> usize {
    read_positive_env(
        "AINS_TOOL_OUTPUT_INLINE_CHARS",
        DEFAULT_TOOL_OUTPUT_INLINE_CHARS,
        256,
    )
}

/// preview 预算：`AINS_TOOL_OUTPUT_PREVIEW_CHARS`（下限 128）。
pub fn tool_output_preview_chars() -> usize {
    read_positive_env(
        "AINS_TOOL_OUTPUT_PREVIEW_CHARS",
        DEFAULT_TOOL_OUTPUT_PREVIEW_CHARS,
        128,
    )
}

/// microcompact 判定阈值：`AINS_MICROCOMPACT_TOOL_RESULT_CHARS`（下限 256）。
pub fn microcompact_tool_result_chars() -> usize {
    read_positive_env(
        "AINS_MICROCOMPACT_TOOL_RESULT_CHARS",
        DEFAULT_MICROCOMPACT_TOOL_RESULT_CHARS,
        256,
    )
}

/// 旧 tool_result 是否可被 microcompact 清理（对齐
/// `is_microcompactable_tool_result`）：MCP 工具恒可清理，其余按阈值。
pub fn is_microcompactable_tool_result(tool_name: &str, content: &str) -> bool {
    if tool_name.trim().starts_with("mcp__") {
        return true;
    }
    content.chars().count() >= microcompact_tool_result_chars()
}

#[cfg(not(target_arch = "wasm32"))]
fn read_positive_env(name: &str, default: usize, minimum: usize) -> usize {
    parse_positive_value(name, std::env::var(name).ok().as_deref(), default, minimum)
}

#[cfg(target_arch = "wasm32")]
fn read_positive_env(_name: &str, default: usize, _minimum: usize) -> usize {
    default
}

/// 环境变量原始值解析（纯函数，供单测直接覆盖：测试进程内
/// set_var/remove_var 与并行测试竞争且在 glibc 下构成 UB，review 二轮修复）。
/// WASM 上无进程环境，仅 Native 读取入口调用。
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn parse_positive_value(name: &str, raw: Option<&str>, default: usize, minimum: usize) -> usize {
    let Some(raw) = raw else {
        return default;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<usize>() {
        Ok(value) => value.max(minimum),
        Err(_) => {
            tracing::warn!("Ignoring invalid {name}={raw:?}");
            default
        }
    }
}

/// 超长输出的外置存储端口。Native 默认实现为文件（`FsArtifactSink`）；
/// Web 宿主可注入 KvStore 后端，未注入时超长输出仅保留内联预览。
pub trait ArtifactSink: MaybeSendSync {
    /// 存储全文，返回可读引用（文件路径 / 存储键）。
    fn store(&self, tool_name: &str, tool_use_id: &str, output: &str) -> Result<String, String>;
}

/// 预算裁决结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffloadedOutput {
    /// 回填 tool_result 的内联文本（原文或截断标记 + 预览）。
    pub inline: String,
    /// 外置存储引用（未超限或无 sink 时为 None）。
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct InlineBudget {
    inline_chars: usize,
    preview_chars: usize,
}

/// 输出预算裁决（对齐 `_offload_tool_output_if_needed`）：
/// 未超 inline 预算原样返回；超限则全文外置 + 生成截断标记与预览。
pub fn offload_tool_output_if_needed(
    tool_name: &str,
    tool_use_id: &str,
    output: String,
    sink: Option<&dyn ArtifactSink>,
) -> OffloadedOutput {
    let total_chars = output.chars().count();
    let inline_limit = tool_output_inline_chars();
    let budget = InlineBudget {
        inline_chars: inline_limit,
        preview_chars: tool_output_preview_chars(),
    };
    if total_chars <= inline_limit {
        return OffloadedOutput {
            inline: output,
            artifact: None,
        };
    }

    let artifact = match sink {
        Some(sink) => match sink.store(tool_name, tool_use_id, &output) {
            Ok(reference) => Some(reference),
            Err(error) => {
                tracing::warn!("failed to store tool output artifact: {error}");
                // store 失败与无 sink 区分文案（review 二轮：误导性诊断）
                return build_truncated_inline(
                    tool_name,
                    tool_use_id,
                    &output,
                    total_chars,
                    None,
                    Some(&error),
                    budget,
                );
            }
        },
        None => None,
    };
    build_truncated_inline(
        tool_name,
        tool_use_id,
        &output,
        total_chars,
        artifact,
        None,
        budget,
    )
}

fn build_truncated_inline(
    tool_name: &str,
    tool_use_id: &str,
    output: &str,
    total_chars: usize,
    artifact: Option<String>,
    store_error: Option<&str>,
    budget: InlineBudget,
) -> OffloadedOutput {
    let preview: String = output
        .chars()
        .take(budget.preview_chars.min(budget.inline_chars))
        .collect();
    let preview_chars = preview.chars().count();
    let omitted = total_chars.saturating_sub(preview_chars);

    let mut inline = format!(
        "[Tool output truncated]\nTool: {tool_name}\nTool use id: {tool_use_id}\n\
         Original size: {total_chars} chars\n"
    );
    match (&artifact, store_error) {
        (Some(reference), _) => inline.push_str(&format!("Full output saved to: {reference}\n")),
        // 有 sink 但落盘失败：文案携带失败原因便于诊断
        (None, Some(error)) => {
            inline.push_str(&format!(
                "Full output not persisted (artifact storage failed: {error})\n"
            ));
        }
        // 无外置存储（如 Web 未注入 sink）：全文不保留，标记说明（偏差记录）
        (None, None) => {
            inline.push_str("Full output not persisted (no artifact storage available)\n");
        }
    }
    inline.push_str(&format!("Inline preview: first {preview_chars} chars"));
    if omitted > 0 {
        inline.push_str(&format!(" ({omitted} chars omitted)"));
    }
    if !preview.is_empty() {
        inline.push_str(&format!("\n\nPreview:\n{preview}"));
    }
    // The diagnostic header is part of the model-facing result too. A large
    // preview, tool id, artifact reference, or sink error must not make the
    // final inline value exceed the configured cap.
    if inline.chars().count() > budget.inline_chars {
        inline = inline.chars().take(budget.inline_chars).collect();
    }
    OffloadedOutput { inline, artifact }
}

/// Native 文件系统 ArtifactSink：`{dir}/{unix_ms}-{safe_tool}-{seq}.txt`。
#[cfg(not(target_arch = "wasm32"))]
pub struct FsArtifactSink {
    dir: std::path::PathBuf,
}

/// 进程级序号：多个 sink 实例指向同目录也不会同毫秒同名碰撞
/// （review 二轮：per-instance 序号在多实例场景可互相覆盖）。
#[cfg(not(target_arch = "wasm32"))]
static ARTIFACT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(not(target_arch = "wasm32"))]
impl FsArtifactSink {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// 文件名安全化（对齐 `_safe_tool_artifact_name`）：非 `[A-Za-z0-9_.-]`
    /// 归一为 `_`，空值回落 `tool`，截断 80 字符。
    fn safe_name(tool_name: &str) -> String {
        let mut normalized = String::new();
        let mut last_was_replacement = false;
        for ch in tool_name.trim().chars() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-') {
                normalized.push(ch);
                last_was_replacement = false;
            } else if !last_was_replacement {
                normalized.push('_');
                last_was_replacement = true;
            }
        }
        if normalized.is_empty() {
            normalized.push_str("tool");
        }
        normalized.chars().take(80).collect()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ArtifactSink for FsArtifactSink {
    fn store(&self, tool_name: &str, _tool_use_id: &str, output: &str) -> Result<String, String> {
        use std::sync::atomic::Ordering;

        std::fs::create_dir_all(&self.dir).map_err(|error| error.to_string())?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        let seq = ARTIFACT_SEQ.fetch_add(1, Ordering::Relaxed);
        let filename = format!("{now_ms}-{}-{seq}.txt", Self::safe_name(tool_name));
        let path = self.dir.join(filename);
        std::fs::write(&path, output).map_err(|error| error.to_string())?;
        Ok(path.display().to_string())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn short_output_passes_through() {
        let result = offload_tool_output_if_needed("grep", "tu_1", "short".into(), None);
        assert_eq!(result.inline, "short");
        assert_eq!(result.artifact, None);
    }

    #[test]
    fn long_output_without_sink_keeps_preview_only() {
        let output = "x".repeat(DEFAULT_TOOL_OUTPUT_INLINE_CHARS + 100);
        let result = offload_tool_output_if_needed("grep", "tu_1", output, None);
        assert!(result.artifact.is_none());
        assert!(result.inline.starts_with("[Tool output truncated]"));
        assert!(result.inline.contains("Tool: grep"));
        assert!(result.inline.contains("Tool use id: tu_1"));
        assert!(result.inline.contains(&format!(
            "Original size: {} chars",
            DEFAULT_TOOL_OUTPUT_INLINE_CHARS + 100
        )));
        assert!(result.inline.contains("no artifact storage available"));
        assert!(result.inline.contains(&format!(
            "Inline preview: first {DEFAULT_TOOL_OUTPUT_PREVIEW_CHARS} chars"
        )));
        // omitted = total - preview
        assert!(result.inline.contains(&format!(
            "({} chars omitted)",
            DEFAULT_TOOL_OUTPUT_INLINE_CHARS + 100 - DEFAULT_TOOL_OUTPUT_PREVIEW_CHARS
        )));
    }

    #[test]
    fn long_output_with_fs_sink_offloads_full_text() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FsArtifactSink::new(dir.path());
        let output = format!(
            "HEAD::{}",
            "y".repeat(DEFAULT_TOOL_OUTPUT_INLINE_CHARS + 50)
        );
        let result =
            offload_tool_output_if_needed("web_fetch", "tu_2", output.clone(), Some(&sink));
        let artifact = result.artifact.expect("artifact reference");
        assert!(
            result
                .inline
                .contains(&format!("Full output saved to: {artifact}"))
        );
        assert!(result.inline.contains("Preview:\nHEAD::"));
        // 全文落盘可回读
        let persisted = std::fs::read_to_string(&artifact).unwrap();
        assert_eq!(persisted, output);
    }

    #[test]
    fn char_budget_counts_scalars_not_bytes() {
        // 多字节字符：字符数在预算内则不截断（字节数远超也不触发）
        let output = "记".repeat(DEFAULT_TOOL_OUTPUT_INLINE_CHARS);
        let result = offload_tool_output_if_needed("t", "tu", output.clone(), None);
        assert_eq!(result.inline, output);
        // 超出 1 字符即触发
        let output = "记".repeat(DEFAULT_TOOL_OUTPUT_INLINE_CHARS + 1);
        let result = offload_tool_output_if_needed("t", "tu", output, None);
        assert!(result.inline.starts_with("[Tool output truncated]"));
    }

    #[test]
    fn final_inline_respects_cap_when_preview_is_larger() {
        let output = "x".repeat(1_000);
        let result = build_truncated_inline(
            "tool",
            "tu",
            &output,
            output.len(),
            None,
            None,
            InlineBudget {
                inline_chars: 256,
                preview_chars: 1_000,
            },
        );
        assert_eq!(result.inline.chars().count(), 256);
        assert!(result.inline.starts_with("[Tool output truncated]"));
        assert!(!result.inline.ends_with(&output));
    }

    #[test]
    fn microcompact_eligibility() {
        assert!(is_microcompactable_tool_result("mcp__srv__tool", "tiny"));
        assert!(!is_microcompactable_tool_result("grep", "tiny"));
        let big = "z".repeat(DEFAULT_MICROCOMPACT_TOOL_RESULT_CHARS);
        assert!(is_microcompactable_tool_result("grep", &big));
    }

    #[test]
    fn safe_artifact_name_normalization() {
        assert_eq!(FsArtifactSink::safe_name("web_fetch"), "web_fetch");
        assert_eq!(FsArtifactSink::safe_name("a b/c"), "a_b_c");
        assert_eq!(FsArtifactSink::safe_name("  "), "tool");
        assert_eq!(FsArtifactSink::safe_name("汉字!!name"), "_name");
        assert_eq!(FsArtifactSink::safe_name(&"n".repeat(120)).len(), 80);
    }

    #[test]
    fn env_override_respects_minimum() {
        // 纯函数直测：不碰进程环境（set_var 与并行测试竞争 + glibc UB，
        // review 二轮修复）
        let name = "AINS_TOOL_OUTPUT_PREVIEW_CHARS";
        assert_eq!(parse_positive_value(name, Some("5"), 3000, 128), 128);
        assert_eq!(
            parse_positive_value(name, Some("not-a-number"), 3000, 128),
            3000
        );
        assert_eq!(parse_positive_value(name, Some("900"), 3000, 128), 900);
        assert_eq!(parse_positive_value(name, Some("  "), 3000, 128), 3000);
        assert_eq!(parse_positive_value(name, None, 3000, 128), 3000);
        // 无环境变量时公开入口回默认值
        assert_eq!(
            tool_output_preview_chars(),
            DEFAULT_TOOL_OUTPUT_PREVIEW_CHARS
        );
    }
}
