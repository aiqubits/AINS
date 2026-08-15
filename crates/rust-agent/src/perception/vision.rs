//! Vision 感知通道（Phase 4.1）：摄像头 / 截屏帧采集 + Server API 调用。
//!
//! 平台采集（摄像头 / 截屏）由前端完成；本通道接收已采集的图像字节，产出
//! Image 附件（供 vision 走带图消息），并提供 `describe` 便捷方法经 `ModelClient`
//! 调用服务端 vision 能力（带 `input_image` 的 chat 消息）得到图像描述文本。

use base64::Engine as _;
use futures::StreamExt;

use crate::error::AgentError;
use crate::kernel::messages::{ContentBlock, ConversationMessage, Role};
use crate::model_client::{ModelClient, ModelRequest, ModelStreamEvent};
use crate::perception::{MAX_IMAGE_BYTES, PerceptionOutcome};
pub use crate::prompts::DEFAULT_VISION_PROMPT;

/// Vision 通道（无状态；采集与模型调用均为方法入口）。
#[derive(Debug, Clone, Copy, Default)]
pub struct VisionChannel;

impl VisionChannel {
    pub fn new() -> Self {
        Self
    }

    /// 采集一帧：校验 `image/*` mime 与大小上限，产出 Image 附件结果。
    /// 采集来源（摄像头 / 截屏）由平台层提供 `data`。
    pub fn capture(&self, data: Vec<u8>, mime_type: &str) -> Result<PerceptionOutcome, AgentError> {
        validate_image(mime_type, data.len())?;
        Ok(PerceptionOutcome::from_image(mime_type, data)
            .with_source_note("[vision: captured frame]"))
    }

    /// 调用服务端 vision 能力描述图像：构造带 `Image` block 的 chat 消息，
    /// 经 `ModelClient` 流式获取完整文本（vision 即带图消息，无独立方法）。
    pub async fn describe(
        &self,
        model: &dyn ModelClient,
        data: &[u8],
        mime_type: &str,
        prompt: Option<&str>,
    ) -> Result<String, AgentError> {
        validate_image(mime_type, data.len())?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        let message = ConversationMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: prompt.unwrap_or(DEFAULT_VISION_PROMPT).to_string(),
                },
                ContentBlock::Image {
                    media_type: mime_type.to_string(),
                    data: encoded,
                },
            ],
        };
        let request = ModelRequest {
            messages: vec![message],
            ..Default::default()
        };
        let mut stream = model.stream_response(request).await?;
        let mut description = String::new();
        while let Some(event) = stream.next().await {
            if let ModelStreamEvent::Complete { message, .. } = event {
                description = message.text();
            }
        }
        if description.trim().is_empty() {
            return Err(AgentError::Model(
                "vision description stream ended without a complete message".into(),
            ));
        }
        Ok(description)
    }
}

/// 校验图像 mime 前缀与大小上限。
fn validate_image(mime_type: &str, len: usize) -> Result<(), AgentError> {
    if !mime_type.starts_with("image/") {
        return Err(AgentError::Model(format!(
            "vision input requires an image/* media type, got {mime_type}"
        )));
    }
    if len == 0 {
        return Err(AgentError::Model("vision input is empty".into()));
    }
    if len > MAX_IMAGE_BYTES {
        return Err(AgentError::Model(format!(
            "vision input exceeds {MAX_IMAGE_BYTES} bytes"
        )));
    }
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::kernel::mock_model::ScriptedModelClient;
    use crate::model_client::UsageSnapshot;

    #[test]
    fn capture_produces_image_attachment_with_note() {
        let outcome = VisionChannel::new()
            .capture(vec![1, 2, 3], "image/png")
            .unwrap();
        assert_eq!(outcome.attachments.len(), 1);
        assert_eq!(outcome.attachments[0].mime_type, "image/png");
        assert_eq!(
            outcome.source_note.as_deref(),
            Some("[vision: captured frame]")
        );
    }

    #[test]
    fn capture_rejects_non_image_and_empty() {
        let channel = VisionChannel::new();
        assert!(channel.capture(vec![1], "text/plain").is_err());
        assert!(channel.capture(vec![], "image/png").is_err());
    }

    #[tokio::test]
    async fn describe_sends_image_message_and_returns_text() {
        let model = ScriptedModelClient::new(vec![ScriptedModelClient::text_turn(
            "a red square",
            UsageSnapshot::default(),
        )]);
        let text = VisionChannel::new()
            .describe(&model, &[1, 2, 3, 4], "image/png", Some("what is this?"))
            .await
            .unwrap();
        assert_eq!(text, "a red square");
        // 断言请求消息携带 Image block
        let requests = model.recorded_requests();
        assert_eq!(requests.len(), 1);
        let has_image = requests[0].messages[0]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }));
        assert!(has_image);
    }

    #[tokio::test]
    async fn describe_rejects_non_image() {
        let model = ScriptedModelClient::new(vec![]);
        let err = VisionChannel::new()
            .describe(&model, &[1], "application/pdf", None)
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn describe_propagates_model_error() {
        // 空脚本：stream_response 返回错误 → describe 应将其传播而非静默成功
        let model = ScriptedModelClient::new(vec![]);
        let err = VisionChannel::new()
            .describe(&model, &[1, 2, 3, 4], "image/png", None)
            .await;
        assert!(err.is_err());
    }
}
