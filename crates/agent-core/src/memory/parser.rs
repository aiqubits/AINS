//! 文档解析与语义分块（AINS_PLAN 4.3 处理流程：Parser → Chunk Splitter）。
//!
//! - Markdown：按标题层级切分（pulldown-cmark，双 target）；
//! - Plain Text：按段落/空行切分（双 target）;
//! - Code：按空行块启发式切分（tree-sitter AST 感知分块留待后续 Phase，
//!   偏差记录见 docs/alignment/phase2-embedded-memory.md）；
//! - PDF：文本提取仅 Native（pdf-extract）；Web 端返回不支持错误。
//!
//! 分块目标 512 tokens/chunk，以 4 chars/token 估算为字符预算。

use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::error::MemoryError;

/// 目标 chunk 大小（tokens）。
pub const CHUNK_TARGET_TOKENS: usize = 512;
/// 粗略 token 估算：4 字符 ≈ 1 token。
pub const CHARS_PER_TOKEN: usize = 4;
/// 单 chunk 字符预算。
pub const MAX_CHUNK_CHARS: usize = CHUNK_TARGET_TOKENS * CHARS_PER_TOKEN;

/// 文档类型（按扩展名推断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    PlainText,
    Markdown,
    Code,
    Pdf,
}

impl DocumentKind {
    pub fn from_name(name: &str) -> Self {
        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
        match ext.as_str() {
            "md" | "markdown" => Self::Markdown,
            "pdf" => Self::Pdf,
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "cc" | "cpp"
            | "h" | "hpp" | "cs" | "rb" | "php" | "swift" | "kt" | "scala" | "sh" | "sql"
            | "toml" | "yaml" | "yml" | "json" => Self::Code,
            _ => Self::PlainText,
        }
    }
}

/// 将文本按空行分段后打包为不超过 `MAX_CHUNK_CHARS` 的 chunk；
/// 超长段落按字符边界硬切。
fn pack_paragraphs(paragraphs: Vec<&str>) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for para in paragraphs {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if !current.is_empty() && current.len() + para.len() + 2 > MAX_CHUNK_CHARS {
            chunks.push(std::mem::take(&mut current));
        }
        if para.len() > MAX_CHUNK_CHARS {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            // 超长段落按 char 边界硬切
            let mut rest = para;
            while !rest.is_empty() {
                let mut cut = rest.len().min(MAX_CHUNK_CHARS);
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                chunks.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            continue;
        }
        if current.is_empty() {
            current.push_str(para);
        } else {
            current.push_str("\n\n");
            current.push_str(para);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Plain Text / Code：按段落（空行）切分。
fn chunk_text(text: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // 空行边界：\n 后跟（可含空白的）\n
        if bytes[i] == b'\n' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\n' {
                paragraphs.push(&text[start..i]);
                start = j + 1;
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    if start < text.len() {
        paragraphs.push(&text[start..]);
    }
    pack_paragraphs(paragraphs)
}

/// Markdown：按标题层级切分为章节，章节内再按段落预算打包。
fn chunk_markdown(text: &str) -> Vec<String> {
    let parser = Parser::new_ext(text, Options::empty());
    let mut boundaries = vec![0usize];
    for (event, range) in parser.into_offset_iter() {
        if let Event::Start(Tag::Heading { .. }) = event
            && range.start > 0
        {
            boundaries.push(range.start);
        }
    }
    boundaries.push(text.len());
    boundaries.dedup();

    let mut chunks = Vec::new();
    for window in boundaries.windows(2) {
        let section = text[window[0]..window[1]].trim();
        if section.is_empty() {
            continue;
        }
        if section.len() <= MAX_CHUNK_CHARS {
            chunks.push(section.to_string());
        } else {
            chunks.extend(chunk_text(section));
        }
    }
    chunks
}

/// 解析并分块文本类文档（PlainText / Markdown / Code）。
pub fn chunk_document(kind: DocumentKind, text: &str) -> Vec<String> {
    match kind {
        DocumentKind::Markdown => chunk_markdown(text),
        DocumentKind::PlainText | DocumentKind::Code => chunk_text(text),
        // PDF 先经 extract_pdf_text 提取为纯文本
        DocumentKind::Pdf => chunk_text(text),
    }
}

/// PDF 文本提取（仅 Native）。
#[cfg(not(target_arch = "wasm32"))]
pub fn extract_pdf_text(bytes: &[u8]) -> Result<String, MemoryError> {
    pdf_extract::extract_text_from_mem(bytes)
        .map_err(|e| MemoryError::Storage(format!("pdf extract failed: {e}")))
}

/// PDF 文本提取在 Web 端不可用。
#[cfg(target_arch = "wasm32")]
pub fn extract_pdf_text(_bytes: &[u8]) -> Result<String, MemoryError> {
    Err(MemoryError::Storage(
        "pdf parsing is not supported on web".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{DocumentKind, MAX_CHUNK_CHARS, chunk_document};

    /// 构造最小单页 PDF（Helvetica 单段文本，xref 偏移运行时计算）。
    #[cfg(not(target_arch = "wasm32"))]
    fn build_minimal_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{body}\nendobj\n", index + 1).into_bytes());
        }
        let xref_offset = pdf.len();
        let mut xref = String::from("xref\n0 6\n0000000000 65535 f \n");
        for offset in &offsets {
            xref.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.extend(xref.into_bytes());
        pdf.extend(
            format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF")
                .into_bytes(),
        );
        pdf
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn pdf_extraction_roundtrip_minimal_document() {
        // pdf-extract 0.7→0.12（lopdf 0.42，RUSTSEC-2026-0187 修复）升级回归：
        // 最小单页 PDF 的文本提取行为不得回退。
        let pdf = build_minimal_pdf("Hello AINS");
        let text = super::extract_pdf_text(&pdf).unwrap();
        assert!(text.contains("Hello AINS"), "extracted: {text:?}");
        // 损坏输入：错误传导为 MemoryError，不得 panic
        assert!(super::extract_pdf_text(b"%PDF-1.4 broken").is_err());
    }

    #[test]
    fn kind_from_name_by_extension() {
        assert_eq!(DocumentKind::from_name("a.md"), DocumentKind::Markdown);
        assert_eq!(DocumentKind::from_name("b.PDF"), DocumentKind::Pdf);
        assert_eq!(DocumentKind::from_name("c.rs"), DocumentKind::Code);
        assert_eq!(DocumentKind::from_name("README"), DocumentKind::PlainText);
        assert_eq!(
            DocumentKind::from_name("notes.txt"),
            DocumentKind::PlainText
        );
    }

    #[test]
    fn markdown_splits_on_headings() {
        let text = "# Setup\n\nInstall rustup.\n\n# Testing\n\nRun cargo test.";
        let chunks = chunk_document(DocumentKind::Markdown, text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].starts_with("# Setup"));
        assert!(chunks[1].starts_with("# Testing"));
    }

    #[test]
    fn plain_text_packs_paragraphs_within_budget() {
        // 20 个小段落打包：每个 chunk 不超预算，内容无丢失
        let para = "x".repeat(300);
        let text = vec![para.clone(); 20].join("\n\n");
        let chunks = chunk_document(DocumentKind::PlainText, &text);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= MAX_CHUNK_CHARS);
        }
        let total: usize = chunks.iter().map(|c| c.matches('x').count()).sum();
        assert_eq!(total, 300 * 20);
    }

    #[test]
    fn oversized_cjk_paragraph_hard_cuts_at_char_boundary() {
        // 单段落 3000 字 × 3 字节：硬切不得 panic（char boundary）、不丢字
        let text = "记".repeat(3000);
        let chunks = chunk_document(DocumentKind::PlainText, &text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= MAX_CHUNK_CHARS);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn empty_and_whitespace_only_produce_no_chunks() {
        assert!(chunk_document(DocumentKind::PlainText, "").is_empty());
        assert!(chunk_document(DocumentKind::PlainText, "  \n\n  \n").is_empty());
        assert!(chunk_document(DocumentKind::Markdown, "\n\n").is_empty());
    }

    #[test]
    fn crlf_blank_lines_split_paragraphs() {
        let text = "first para\r\n\r\nsecond para";
        let chunks = chunk_document(DocumentKind::Code, text);
        assert_eq!(chunks.len(), 1);
        // 段落边界被识别（\r 视为空白），重新打包为双换行分隔
        assert_eq!(chunks[0], "first para\n\nsecond para");
    }
}
