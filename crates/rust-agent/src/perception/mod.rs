//! 感知系统（Phase 4）：Vision / Voice / File 三类输入通道，将平台采集的
//! 原始数据归一化为可注入会话的内容，并写入 `ContextStore`（4.4）。
//!
//! 架构分工：平台特定采集（摄像头 / 麦克风 / 截屏 / 拖拽）由各前端
//! （app/web、app/desktop）在其平台 API 层完成；本模块只接收已采集的原始
//! 字节，保持 rust-agent 纯粹、双端可测。感知结果统一为 [`PerceptionOutcome`]，
//! 经 [`PerceptionOutcome::into_agent_event`] 转为 `AgentEvent::UserMessage`
//! 后由既有 `ContextStore::build` 落入上下文（图像附件 → Image block）。

pub mod file;
pub mod vision;
pub mod voice;

pub use file::{FileChannel, MAX_FILE_TEXT_CHARS};
pub use vision::VisionChannel;
pub use voice::VoiceChannel;

use crate::error::AgentError;
use crate::kernel::context::ContextStore;
use crate::kernel::state::{AgentEvent, Attachment};

/// 单张图像的最大字节数（采集/拖拽护栏，避免超大帧撑爆内存与请求）。
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// 归一化的感知结果：可注入会话的文本 + 图像附件 + 来源说明。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PerceptionOutcome {
    /// 抽取 / 转写得到的文本（File 文本解析、Voice STT）。
    pub text: Option<String>,
    /// 图像附件（Vision 采集、File 图像拖拽）；mime 为 `image/*`。
    pub attachments: Vec<Attachment>,
    /// 来源说明（如 `[file: notes.pdf]`），拼入消息前缀，供模型溯源。
    pub source_note: Option<String>,
}

impl PerceptionOutcome {
    /// 纯文本结果。
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Default::default()
        }
    }

    /// 单图像附件结果。
    pub fn from_image(mime_type: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            attachments: vec![Attachment {
                mime_type: mime_type.into(),
                data,
            }],
            ..Default::default()
        }
    }

    /// 附加来源说明（builder）。
    pub fn with_source_note(mut self, note: impl Into<String>) -> Self {
        self.source_note = Some(note.into());
        self
    }

    /// 是否无任何可注入内容（空文本且无附件）。
    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty() && self.text.as_ref().is_none_or(|t| t.trim().is_empty())
    }

    /// 组合最终消息文本：`[user_prompt] + [source_note] + [text]`（按序，空段跳过）。
    fn compose_content(&self, user_prompt: Option<&str>) -> String {
        let mut segments: Vec<&str> = Vec::new();
        if let Some(prompt) = user_prompt
            && !prompt.trim().is_empty()
        {
            segments.push(prompt.trim());
        }
        if let Some(note) = &self.source_note
            && !note.trim().is_empty()
        {
            segments.push(note.trim());
        }
        if let Some(text) = &self.text
            && !text.trim().is_empty()
        {
            segments.push(text.trim());
        }
        segments.join("\n\n")
    }

    /// 转为 `AgentEvent::UserMessage`（可携带可选用户文本）；无内容返回 `None`。
    pub fn into_agent_event(self, user_prompt: Option<&str>) -> Option<AgentEvent> {
        if self.is_empty() && user_prompt.is_none_or(|p| p.trim().is_empty()) {
            return None;
        }
        let content = self.compose_content(user_prompt);
        Some(AgentEvent::UserMessage {
            content,
            attachments: self.attachments,
        })
    }

    /// 直接落入 `ContextStore`（4.4）：转事件后复用既有 `ContextStore::build`
    /// （图像附件 → Image block，文本 → Text block + user_goal）。
    pub async fn apply_to_context(
        self,
        ctx: &mut ContextStore,
        user_prompt: Option<&str>,
    ) -> Result<bool, AgentError> {
        match self.into_agent_event(user_prompt) {
            Some(event) => {
                ctx.build(&event).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::kernel::messages::{ContentBlock, Role};

    #[test]
    fn compose_content_orders_prompt_note_text() {
        let outcome =
            PerceptionOutcome::from_text("extracted body").with_source_note("[file: a.txt]");
        let content = outcome.compose_content(Some("summarize this"));
        assert_eq!(content, "summarize this\n\n[file: a.txt]\n\nextracted body");
    }

    #[test]
    fn into_agent_event_none_when_fully_empty() {
        let outcome = PerceptionOutcome::default();
        assert!(outcome.clone().into_agent_event(None).is_none());
        assert!(outcome.into_agent_event(Some("   ")).is_none());
    }

    #[test]
    fn into_agent_event_carries_image_attachment() {
        let outcome = PerceptionOutcome::from_image("image/png", vec![1, 2, 3]);
        let event = outcome.into_agent_event(Some("what is this")).unwrap();
        match event {
            AgentEvent::UserMessage {
                content,
                attachments,
            } => {
                assert_eq!(content, "what is this");
                assert_eq!(attachments.len(), 1);
                assert_eq!(attachments[0].mime_type, "image/png");
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[tokio::test]
    async fn apply_to_context_appends_text_and_image_blocks() {
        let mut ctx = ContextStore::new();
        let outcome = PerceptionOutcome {
            text: Some("transcribed speech".into()),
            attachments: vec![Attachment {
                mime_type: "image/png".into(),
                data: vec![9, 9],
            }],
            source_note: Some("[voice + frame]".into()),
        };
        let applied = outcome.apply_to_context(&mut ctx, None).await.unwrap();
        assert!(applied);
        assert_eq!(ctx.conversation.len(), 1);
        let message = &ctx.conversation[0];
        assert_eq!(message.role, Role::User);
        // 文本块 + 图像块
        assert!(message.content.iter().any(
            |b| matches!(b, ContentBlock::Text { text } if text.contains("transcribed speech"))
        ));
        assert!(
            message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
        );
    }

    #[tokio::test]
    async fn apply_to_context_noop_when_empty() {
        let mut ctx = ContextStore::new();
        let applied = PerceptionOutcome::default()
            .apply_to_context(&mut ctx, None)
            .await
            .unwrap();
        assert!(!applied);
        assert!(ctx.conversation.is_empty());
    }
}
