//! 消息与内容块（对齐 OpenHarness `engine/messages.py` 的判别式 content block 模型）。
//!
//! wire 形状：`{"type": "text" | "image" | "tool_use" | "tool_result", ...}`。

use std::collections::HashSet;

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
        /// 工具特定的结构化结果（对齐 OpenHarness `result_metadata`）。
        #[serde(default)]
        result_metadata: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 模型请求的单个工具调用（从 assistant 消息的 `ToolUse` block 提取）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl ConversationMessage {
    /// 单条纯文本用户消息（对齐基线 `from_user_text`）。
    pub fn from_user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 由 content block 列表构造用户消息（对齐基线 `from_user_content`）。
    pub fn from_user_content(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
        }
    }

    /// 全部 Text block 文本的无分隔拼接（对齐基线 `text` property）。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 按内容顺序提取全部 `ToolUse` block（对齐基线 `tool_uses` property）。
    pub fn tool_uses(&self) -> Vec<ToolUse> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some(ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// 是否为“实质空”消息（对齐基线 `is_effectively_empty`）：
    /// 仅当存在非空白 Text，或任意 Image / ToolUse / ToolResult block 时非空。
    pub fn is_effectively_empty(&self) -> bool {
        !self.content.iter().any(|block| match block {
            ContentBlock::Text { text } => !text.trim().is_empty(),
            ContentBlock::Image { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => true,
        })
    }
}

/// 历史修剪（精确对齐基线 `sanitize_conversation_messages`）：
///
/// 1. 丢弃实质空的 assistant 消息；
/// 2. assistant `tool_use` 若未被紧随其后的 user 消息中的 `tool_result`
///    全量覆盖（pending ids ⊆ result ids），回溯删除该 assistant 消息；
/// 3. 剔除孤儿 `tool_result`（无配对 pending tool_use）；整条消息被剔空则丢弃；
/// 4. 收尾修剪末尾悬空的 `tool_use` 消息。
///
/// 会话中断恢复与快照回载前必调（`submit` / `continue_pending` 两路径均先 sanitize）。
pub fn sanitize_conversation_messages(
    messages: Vec<ConversationMessage>,
) -> Vec<ConversationMessage> {
    let mut sanitized: Vec<ConversationMessage> = Vec::new();
    let mut pending_tool_use_ids: HashSet<String> = HashSet::new();
    let mut pending_tool_use_index: Option<usize> = None;

    for message in messages {
        if message.role == Role::Assistant && message.is_effectively_empty() {
            continue;
        }

        let result_ids: HashSet<&str> = if message.role == Role::User {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    _ => None,
                })
                .collect()
        } else {
            HashSet::new()
        };

        let mut matched_pending_tool_results = false;
        if !pending_tool_use_ids.is_empty() {
            let covers_pending = message.role == Role::User
                && pending_tool_use_ids
                    .iter()
                    .all(|id| result_ids.contains(id.as_str()));
            if covers_pending {
                matched_pending_tool_results = true;
            } else if let Some(index) = pending_tool_use_index
                && index < sanitized.len()
            {
                sanitized.remove(index);
            }
            pending_tool_use_ids.clear();
            pending_tool_use_index = None;
        }

        let message = if message.role == Role::User
            && !result_ids.is_empty()
            && !matched_pending_tool_results
        {
            let remaining: Vec<ContentBlock> = message
                .content
                .into_iter()
                .filter(|block| !matches!(block, ContentBlock::ToolResult { .. }))
                .collect();
            if remaining.is_empty() {
                continue;
            }
            ConversationMessage {
                role: Role::User,
                content: remaining,
            }
        } else {
            message
        };

        sanitized.push(message);

        let last = sanitized.last().expect("just pushed");
        if last.role == Role::Assistant {
            let uses = last.tool_uses();
            if !uses.is_empty() {
                pending_tool_use_ids = uses.into_iter().map(|u| u.id).collect();
                pending_tool_use_index = Some(sanitized.len() - 1);
            }
        }
    }

    if !pending_tool_use_ids.is_empty()
        && let Some(index) = pending_tool_use_index
        && index < sanitized.len()
    {
        sanitized.remove(index);
    }

    sanitized
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
                result_metadata: Value::Null,
            }
        );
    }

    fn user_text(text: &str) -> ConversationMessage {
        ConversationMessage::from_user_text(text)
    }

    fn assistant_tool_use(id: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "echo".into(),
                input: serde_json::json!({}),
            }],
        }
    }

    fn user_tool_result(id: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
                result_metadata: Value::Null,
            }],
        }
    }

    #[test]
    fn is_effectively_empty_semantics() {
        assert!(
            ConversationMessage {
                role: Role::Assistant,
                content: vec![],
            }
            .is_effectively_empty()
        );
        assert!(user_text("   ").is_effectively_empty());
        assert!(!user_text("hi").is_effectively_empty());
        assert!(!assistant_tool_use("toolu_1").is_effectively_empty());
        assert!(!user_tool_result("toolu_1").is_effectively_empty());
    }

    #[test]
    fn sanitize_empty_input_returns_empty() {
        assert!(sanitize_conversation_messages(vec![]).is_empty());
    }

    #[test]
    fn sanitize_preserves_complete_tool_turn() {
        let messages = vec![
            user_text("run it"),
            assistant_tool_use("toolu_1"),
            user_tool_result("toolu_1"),
        ];
        assert_eq!(sanitize_conversation_messages(messages.clone()), messages);
    }

    #[test]
    fn sanitize_drops_empty_assistant_messages() {
        let messages = vec![
            user_text("hi"),
            ConversationMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "  ".into() }],
            },
        ];
        assert_eq!(
            sanitize_conversation_messages(messages),
            vec![user_text("hi")]
        );
    }

    #[test]
    fn sanitize_removes_trailing_dangling_tool_use() {
        let messages = vec![user_text("run it"), assistant_tool_use("toolu_1")];
        assert_eq!(
            sanitize_conversation_messages(messages),
            vec![user_text("run it")]
        );
    }

    #[test]
    fn sanitize_removes_unmatched_tool_use_mid_history() {
        let messages = vec![
            user_text("run it"),
            assistant_tool_use("toolu_1"),
            user_text("next question"),
        ];
        assert_eq!(
            sanitize_conversation_messages(messages),
            vec![user_text("run it"), user_text("next question")]
        );
    }

    #[test]
    fn sanitize_strips_orphan_tool_results_but_keeps_text() {
        let messages = vec![
            user_text("hi"),
            ConversationMessage {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_ghost".into(),
                        content: "orphan".into(),
                        is_error: false,
                        result_metadata: Value::Null,
                    },
                    ContentBlock::Text {
                        text: "still here".into(),
                    },
                ],
            },
        ];
        assert_eq!(
            sanitize_conversation_messages(messages),
            vec![user_text("hi"), user_text("still here")]
        );
    }

    #[test]
    fn sanitize_drops_message_left_empty_after_orphan_strip() {
        let messages = vec![user_text("hi"), user_tool_result("toolu_ghost")];
        assert_eq!(
            sanitize_conversation_messages(messages),
            vec![user_text("hi")]
        );
    }

    #[test]
    fn sanitize_matches_when_results_are_superset_of_pending() {
        let assistant = assistant_tool_use("toolu_1");
        let results = ConversationMessage {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "ok".into(),
                    is_error: false,
                    result_metadata: Value::Null,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "toolu_extra".into(),
                    content: "extra".into(),
                    is_error: false,
                    result_metadata: Value::Null,
                },
            ],
        };
        let messages = vec![user_text("run"), assistant.clone(), results.clone()];
        assert_eq!(
            sanitize_conversation_messages(messages),
            vec![user_text("run"), assistant, results]
        );
    }

    #[test]
    fn sanitize_removes_assistant_when_results_cover_only_subset() {
        let assistant = ConversationMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::ToolUse {
                    id: "toolu_2".into(),
                    name: "echo".into(),
                    input: serde_json::json!({}),
                },
            ],
        };
        // 只回填了 toolu_1，pending ⊄ results → assistant 回溯删除，孤儿 result 也被剔除
        let messages = vec![user_text("run"), assistant, user_tool_result("toolu_1")];
        assert_eq!(
            sanitize_conversation_messages(messages),
            vec![user_text("run")]
        );
    }

    #[test]
    fn sanitize_handles_consecutive_assistant_messages() {
        // 连续两个 assistant 消息（toolu_1 无配对，toolu_2 有配对）
        // → 第一个 assistant 被回溯删除，第二个和其 result 保留
        let messages = vec![
            user_text("run"),
            assistant_tool_use("toolu_1"),
            assistant_tool_use("toolu_2"),
            user_tool_result("toolu_2"),
        ];
        let result = sanitize_conversation_messages(messages);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], user_text("run"));
        assert_eq!(result[1], assistant_tool_use("toolu_2"));
        assert_eq!(result[2], user_tool_result("toolu_2"));
    }
}
