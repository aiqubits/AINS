//! File 感知通道（Phase 4.3）：拖拽文件解析（PDF / Image / Text / Code）。
//!
//! 复用 memory 层的文档解析（`DocumentKind` / `extract_pdf_text`）：
//!
//! - 图像文件 → Image 附件（走 vision）；
//! - PDF → 抽取文本；Text / Code / Markdown → UTF-8 解码；
//!
//! 文本按 [`MAX_FILE_TEXT_CHARS`] 截断，附来源说明 `[file: <name>]`。

use crate::error::AgentError;
use crate::memory::{DocumentKind, extract_pdf_text};
use crate::perception::{MAX_IMAGE_BYTES, PerceptionOutcome};

/// 拖拽文件文本注入的最大字符数（护栏，超长截断）。
pub const MAX_FILE_TEXT_CHARS: usize = 200_000;

/// 非图像文件的最大输入字节数（与 voice 通道 `MAX_AUDIO_BYTES` 同档；
/// 在解码/PDF 抽取前拒绝，避免对超大输入做全量 lossy 解码二次分配）。
pub const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;

/// 文本截断标记。
const FILE_TRUNCATION_MARKER: &str = "\n...[file content truncated]...";

/// File 通道（无状态；解析入口按文件名 + 可选 mime 提示分派）。
#[derive(Debug, Clone, Copy, Default)]
pub struct FileChannel;

impl FileChannel {
    pub fn new() -> Self {
        Self
    }

    /// 解析拖拽文件为感知结果。
    ///
    /// - 图像（mime `image/*` 或图像扩展名）→ Image 附件；
    /// - PDF → 抽取文本（Web 端无 PDF 解析，返回错误）；
    /// - 其余（Text / Code / Markdown）→ UTF-8 解码（有损）+ 截断。
    pub fn ingest(
        &self,
        data: Vec<u8>,
        filename: &str,
        mime_hint: Option<&str>,
    ) -> Result<PerceptionOutcome, AgentError> {
        if data.is_empty() {
            return Err(AgentError::Model(format!("file '{filename}' is empty")));
        }
        let note = format!("[file: {filename}]");

        if is_image(filename, mime_hint) {
            if data.len() > MAX_IMAGE_BYTES {
                return Err(AgentError::Model(format!(
                    "image file '{filename}' exceeds {MAX_IMAGE_BYTES} bytes"
                )));
            }
            let mime = image_mime(filename, mime_hint);
            return Ok(PerceptionOutcome::from_image(mime, data).with_source_note(note));
        }

        let kind = DocumentKind::from_name(filename);
        // 非图像分支的输入字节护栏（图像分支已由 MAX_IMAGE_BYTES 覆盖）
        if data.len() > MAX_FILE_BYTES {
            return Err(AgentError::Model(format!(
                "file '{filename}' exceeds {MAX_FILE_BYTES} bytes"
            )));
        }
        let text = match kind {
            DocumentKind::Pdf => extract_pdf_text(&data).map_err(AgentError::from)?,
            DocumentKind::PlainText | DocumentKind::Code | DocumentKind::Markdown => {
                // NUL 字节是二进制文件的强信号（文本/代码/Markdown 不含）；
                // 拒绝而非向模型上下文注入乱码文本。
                if data.contains(&0) {
                    return Err(AgentError::Model(format!(
                        "file '{filename}' appears to be binary (contains NUL bytes); \
                         only text/code/markdown/pdf/image files are supported"
                    )));
                }
                String::from_utf8_lossy(&data).into_owned()
            }
        };
        if text.trim().is_empty() {
            return Err(AgentError::Model(format!(
                "file '{filename}' produced no extractable text"
            )));
        }
        Ok(PerceptionOutcome::from_text(truncate_text(&text)).with_source_note(note))
    }
}

/// 按字符截断文件文本（超限追加标记）。
fn truncate_text(text: &str) -> String {
    if text.chars().count() <= MAX_FILE_TEXT_CHARS {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX_FILE_TEXT_CHARS).collect();
    format!("{}{FILE_TRUNCATION_MARKER}", truncated.trim_end())
}

/// 是否为图像：mime 提示以 `image/` 开头，或扩展名在图像集合内。
///
/// 已知限制：`svg` 按对齐清单归入图像集合（media_type `image/svg+xml`），
/// 但部分上游 vision 供应商拒收 SVG，失败将以上游错误信封透传回调用方
/// （非静默丢失）；是否收窄由服务端通道能力矩阵决策，客户端不预判。
fn is_image(filename: &str, mime_hint: Option<&str>) -> bool {
    if mime_hint.is_some_and(|m| m.starts_with("image/")) {
        return true;
    }
    matches!(
        extension(filename).as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
    )
}

/// 推断图像 mime：优先 mime 提示，其次按扩展名映射。
fn image_mime(filename: &str, mime_hint: Option<&str>) -> String {
    if let Some(mime) = mime_hint.filter(|m| m.starts_with("image/")) {
        return mime.to_string();
    }
    let subtype = match extension(filename).as_str() {
        "jpg" | "jpeg" => "jpeg",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        "svg" => "svg+xml",
        _ => "png",
    };
    format!("image/{subtype}")
}

/// 扩展名（小写）；无点或点在首位（如 `.gitignore`、无扩展名文件）返回
/// 空，避免把文件名整体误当扩展名（如名为 `png` 的文件误判为图像）。
fn extension(filename: &str) -> String {
    match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn ingest_text_file_produces_text_with_note() {
        let outcome = FileChannel::new()
            .ingest(b"hello world".to_vec(), "notes.txt", None)
            .unwrap();
        assert_eq!(outcome.text.as_deref(), Some("hello world"));
        assert_eq!(outcome.source_note.as_deref(), Some("[file: notes.txt]"));
        assert!(outcome.attachments.is_empty());
    }

    #[test]
    fn ingest_code_file_is_text() {
        let outcome = FileChannel::new()
            .ingest(b"fn main() {}".to_vec(), "main.rs", None)
            .unwrap();
        assert!(outcome.text.as_deref().unwrap().contains("fn main"));
    }

    #[test]
    fn ingest_image_by_extension_produces_attachment() {
        let outcome = FileChannel::new()
            .ingest(vec![1, 2, 3], "photo.jpg", None)
            .unwrap();
        assert_eq!(outcome.attachments.len(), 1);
        assert_eq!(outcome.attachments[0].mime_type, "image/jpeg");
        assert!(outcome.text.is_none());
    }

    #[test]
    fn ingest_image_by_mime_hint() {
        let outcome = FileChannel::new()
            .ingest(vec![1, 2, 3], "frame", Some("image/webp"))
            .unwrap();
        assert_eq!(outcome.attachments[0].mime_type, "image/webp");
    }

    #[test]
    fn ingest_rejects_empty_file() {
        assert!(
            FileChannel::new()
                .ingest(vec![], "empty.txt", None)
                .is_err()
        );
    }

    #[test]
    fn ingest_truncates_oversized_text() {
        let big = "a".repeat(MAX_FILE_TEXT_CHARS + 100);
        let outcome = FileChannel::new()
            .ingest(big.into_bytes(), "big.txt", None)
            .unwrap();
        let text = outcome.text.unwrap();
        assert!(text.ends_with(FILE_TRUNCATION_MARKER));
    }

    #[test]
    fn ingest_rejects_oversized_non_image_file_before_decode() {
        // 回归：非图像分支曾无输入字节上限，超大输入先全量 lossy 解码
        // 再按字符截断；应在解码/PDF 抽取前拒绝
        let oversized = vec![b'a'; MAX_FILE_BYTES + 1];
        let err = FileChannel::new().ingest(oversized, "huge.txt", None);
        assert!(
            err.is_err(),
            "non-image file above MAX_FILE_BYTES must be rejected"
        );
    }

    #[test]
    fn ingest_rejects_whitespace_only_text() {
        assert!(
            FileChannel::new()
                .ingest(b"   \n  ".to_vec(), "blank.txt", None)
                .is_err()
        );
    }

    #[test]
    fn ingest_rejects_binary_with_nul_bytes() {
        // 含 NUL 字节的非图像/非 PDF 文件被当作二进制拒绝，而非乱码解码
        let binary = vec![b'M', b'Z', 0x00, 0x01, 0x02, 0x00, 0xFF];
        let err = FileChannel::new().ingest(binary, "payload.txt", None);
        assert!(err.is_err(), "binary file with NUL bytes must be rejected");
    }

    #[test]
    fn ingest_extensionless_filename_matching_image_ext_is_not_image() {
        // 回归：无扩展名文件名恰为图像扩展名（如 "png"）时，旧 extension()
        // 返回整个文件名，导致误判为图像并以伪造 mime 发送
        let outcome = FileChannel::new()
            .ingest(b"just some text".to_vec(), "png", None)
            .unwrap();
        assert!(
            outcome.attachments.is_empty(),
            "must not be treated as an image"
        );
        assert_eq!(outcome.text.as_deref(), Some("just some text"));
    }

    #[test]
    fn extension_requires_non_empty_stem() {
        assert_eq!(extension("a.PNG"), "png");
        assert_eq!(extension("archive.tar.gz"), "gz");
        assert_eq!(extension("png"), "");
        assert_eq!(extension(".gitignore"), "");
    }
}
