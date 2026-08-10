//! 会话上下文管理（AINS_PLAN 3.1 `ContextStore`）：
//! 对话历史（content block）+ 跨轮 tool_metadata 状态袋 + 激活的 skill 摘要。
//!
//! 超阈值压缩由 context/compact（后续 Phase）处理，不做头部直接截断。

use base64::Engine as _;

use crate::error::AgentError;
use crate::kernel::messages::{ContentBlock, ConversationMessage, Role};
use crate::kernel::state::AgentEvent;
use crate::skills::SkillSummary;
use crate::tools::ToolMetadata;

#[derive(Debug, Default)]
pub struct ContextStore {
    /// 完整对话历史；回载/追加前由调用方先过 `sanitize_conversation_messages`。
    pub conversation: Vec<ConversationMessage>,
    /// 跨轮状态袋：已读文件/已调技能/用户目标/工作日志（各键条数上限）。
    pub tool_metadata: ToolMetadata,
    /// 当前激活的 skill 摘要（Skills 渐进式加载于 Phase 4 接入）。
    pub loaded_skills: Vec<SkillSummary>,
}

impl ContextStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从事件更新上下文。
    ///
    /// - `UserMessage`：文本 → Text block；`image/*` 附件 → Image block（base64）；
    ///   非图片附件的文档索引在 Phase 2（Document Memory）异步接入，当前忽略。
    ///   非空文本同时记入 tool_metadata 用户目标（对齐基线 `remember_user_goal`
    ///   的最小语义）。
    /// - `SystemEvent`：不修改会话。
    pub async fn build(&mut self, event: &AgentEvent) -> Result<(), AgentError> {
        match event {
            AgentEvent::UserMessage {
                content,
                attachments,
            } => {
                let mut blocks = Vec::new();
                if !content.trim().is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: content.clone(),
                    });
                }
                for attachment in attachments {
                    if attachment.mime_type.starts_with("image/") {
                        blocks.push(ContentBlock::Image {
                            media_type: attachment.mime_type.clone(),
                            data: base64::engine::general_purpose::STANDARD
                                .encode(&attachment.data),
                        });
                    }
                }
                if blocks.is_empty() {
                    return Ok(());
                }
                self.conversation.push(ConversationMessage {
                    role: Role::User,
                    content: blocks,
                });
                if !content.trim().is_empty() {
                    self.tool_metadata.set_user_goal(content.trim());
                }
            }
            AgentEvent::SystemEvent { .. } => {}
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::kernel::state::Attachment;

    #[tokio::test]
    async fn build_appends_user_message_with_image_attachment() {
        let mut store = ContextStore::new();
        store
            .build(&AgentEvent::UserMessage {
                content: "看看这张图".into(),
                attachments: vec![Attachment {
                    mime_type: "image/png".into(),
                    data: vec![1, 2, 3],
                }],
            })
            .await
            .unwrap();
        assert_eq!(store.conversation.len(), 1);
        let message = &store.conversation[0];
        assert_eq!(message.role, Role::User);
        assert_eq!(message.content.len(), 2);
        assert!(matches!(
            &message.content[1],
            ContentBlock::Image { media_type, data }
                if media_type == "image/png" && data == "AQID"
        ));
        assert_eq!(store.tool_metadata.user_goal.as_deref(), Some("看看这张图"));
    }

    #[tokio::test]
    async fn build_skips_blank_prompt_and_non_image_attachments() {
        let mut store = ContextStore::new();
        store
            .build(&AgentEvent::UserMessage {
                content: "   ".into(),
                attachments: vec![Attachment {
                    mime_type: "application/pdf".into(),
                    data: vec![0xFF],
                }],
            })
            .await
            .unwrap();
        assert!(store.conversation.is_empty());
        assert!(store.tool_metadata.user_goal.is_none());
    }

    #[tokio::test]
    async fn build_creates_image_only_message_for_blank_text_with_image() {
        // 空白文本 + 图片附件 → 仅包含 Image block 的 user 消息
        let mut store = ContextStore::new();
        store
            .build(&AgentEvent::UserMessage {
                content: "   ".into(),
                attachments: vec![Attachment {
                    mime_type: "image/png".into(),
                    data: vec![4, 5, 6],
                }],
            })
            .await
            .unwrap();
        assert_eq!(store.conversation.len(), 1);
        let message = &store.conversation[0];
        assert_eq!(message.role, Role::User);
        assert_eq!(message.content.len(), 1);
        assert!(matches!(
            &message.content[0],
            ContentBlock::Image { media_type, data }
                if media_type == "image/png" && data == "BAUG"
        ));
        // 空白文本不设置 user_goal
        assert!(store.tool_metadata.user_goal.is_none());
    }

    #[tokio::test]
    async fn build_ignores_system_events() {
        use crate::kernel::state::SystemEventType;
        let mut store = ContextStore::new();
        store
            .build(&AgentEvent::SystemEvent {
                event_type: SystemEventType::Startup,
            })
            .await
            .unwrap();
        assert!(store.conversation.is_empty());
    }

    #[tokio::test]
    async fn build_appends_multiple_images_in_single_message() {
        // 当一次 UserMessage 包含多个 image 附件时，build 应为每个附件
        // 生成一个 Image block，且 base64 内容正确区分。
        let mut store = ContextStore::new();
        store
            .build(&AgentEvent::UserMessage {
                content: "多张图".into(),
                attachments: vec![
                    Attachment {
                        mime_type: "image/png".into(),
                        data: vec![1, 2, 3],
                    },
                    Attachment {
                        mime_type: "image/jpeg".into(),
                        data: vec![4, 5, 6],
                    },
                ],
            })
            .await
            .unwrap();
        assert_eq!(store.conversation.len(), 1);
        let message = &store.conversation[0];
        assert_eq!(message.content.len(), 3); // text + 2 images
        assert!(matches!(
            &message.content[1],
            ContentBlock::Image { media_type, data }
                if media_type == "image/png" && data == "AQID"
        ));
        assert!(matches!(
            &message.content[2],
            ContentBlock::Image { media_type, data }
                if media_type == "image/jpeg" && data == "BAUG"
        ));
        assert_eq!(store.tool_metadata.user_goal.as_deref(), Some("多张图"));
    }

    #[tokio::test]
    async fn build_creates_image_only_message_with_blank_text_and_multiple_images() {
        // 空白文本 + 多个图片附件 → 仅包含 Image block 的 user 消息，
        // 不含 text block，且不设置 user_goal。
        let mut store = ContextStore::new();
        store
            .build(&AgentEvent::UserMessage {
                content: "   ".into(),
                attachments: vec![
                    Attachment {
                        mime_type: "image/png".into(),
                        data: vec![10, 20, 30],
                    },
                    Attachment {
                        mime_type: "image/webp".into(),
                        data: vec![40, 50, 60],
                    },
                ],
            })
            .await
            .unwrap();
        assert_eq!(store.conversation.len(), 1);
        let message = &store.conversation[0];
        // 空白文本不生成 text block，仅包含 2 个 Image block
        assert_eq!(message.content.len(), 2);
        assert!(matches!(
            &message.content[0],
            ContentBlock::Image { media_type, data }
                if media_type == "image/png" && data == "ChQe"
        ));
        assert!(matches!(
            &message.content[1],
            ContentBlock::Image { media_type, data }
                if media_type == "image/webp" && data == "KDI8"
        ));
        assert!(store.tool_metadata.user_goal.is_none());
    }
}
