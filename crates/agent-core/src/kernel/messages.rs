//! 消息与内容块（对齐 OpenHarness `engine/messages.py` 的判别式 content block 模型）。
//!
//! wire 形状：`{"type": "text" | "image" | "tool_use" | "tool_result", ...}`；
//! `sanitize_conversation_messages`（历史修剪）在 Phase 1 实现。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// `data` 为 base64 编码的图像内容。
    Image {
        media_type: String,
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn content_block_wire_shape_roundtrip() {
        let message = ConversationMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "calculator".into(),
                    input: serde_json::json!({"expr": "1+1"}),
                },
            ],
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][1]["type"], "tool_use");
        let back: ConversationMessage = serde_json::from_value(json).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn tool_result_is_error_defaults_to_false() {
        let block: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": "ok",
        }))
        .unwrap();
        assert_eq!(
            block,
            ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "ok".into(),
                is_error: false,
            }
        );
    }
}
