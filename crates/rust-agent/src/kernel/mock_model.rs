//! 脚本化 Mock ModelClient（对齐 Harness `tests/test_engine` 的 fake client 模式）。
//!
//! 每次 `stream_response` 调用弹出一段预置事件脚本（delta / complete / retry），
//! 同时记录请求（RecordingApiClient 语义），用于驱动工具循环的单测与集成测试。

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::error::AgentError;
use crate::kernel::messages::{ContentBlock, ConversationMessage, Role};
use crate::model_client::{
    EventStream, ModelClient, ModelRequest, ModelStreamEvent, UsageSnapshot,
};

/// 按调用顺序弹出脚本段的 Mock；脚本耗尽后返回 `AgentError::Model`。
#[derive(Default)]
pub struct ScriptedModelClient {
    scripts: Mutex<VecDeque<Vec<ModelStreamEvent>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModelClient {
    pub fn new(scripts: Vec<Vec<ModelStreamEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// 记录的历史请求（对齐 RecordingApiClient：断言 system_prompt / tools / messages）。
    pub fn recorded_requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("mock lock poisoned").clone()
    }

    /// 由完整 assistant 消息生成一段脚本：每个非空 Text block 一条 delta，
    /// 末尾 Complete（对齐 FakeApiClient 的产出顺序）。
    pub fn turn(message: ConversationMessage, usage: UsageSnapshot) -> Vec<ModelStreamEvent> {
        let mut events: Vec<ModelStreamEvent> = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } if !text.is_empty() => {
                    Some(ModelStreamEvent::TextDelta { text: text.clone() })
                }
                _ => None,
            })
            .collect();
        events.push(ModelStreamEvent::Complete {
            message,
            usage,
            stop_reason: None,
        });
        events
    }

    /// 纯文本 assistant turn 的便捷脚本（对齐 StaticApiClient）。
    pub fn text_turn(text: &str, usage: UsageSnapshot) -> Vec<ModelStreamEvent> {
        Self::turn(Self::assistant_text(text), usage)
    }

    /// 构造纯文本 assistant 消息。
    pub fn assistant_text(text: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 构造带 tool_use 的 assistant 消息（可含前置说明文本）。
    pub fn assistant_tool_use(
        text: Option<&str>,
        id: &str,
        name: &str,
        input: serde_json::Value,
    ) -> ConversationMessage {
        let mut content = Vec::new();
        if let Some(text) = text {
            content.push(ContentBlock::Text { text: text.into() });
        }
        content.push(ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        });
        ConversationMessage {
            role: Role::Assistant,
            content,
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ModelClient for ScriptedModelClient {
    async fn stream_response(
        &self,
        request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        self.requests
            .lock()
            .expect("mock lock poisoned")
            .push(request);
        let script = self
            .scripts
            .lock()
            .expect("mock lock poisoned")
            .pop_front()
            .ok_or_else(|| AgentError::Model("scripted model client exhausted".into()))?;
        let stream: EventStream<ModelStreamEvent> = Box::pin(futures::stream::iter(script));
        Ok(stream)
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, AgentError> {
        Err(AgentError::Model("embed not scripted".into()))
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
        Err(AgentError::Model("stt not scripted".into()))
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
        Err(AgentError::Model("tts not scripted".into()))
    }
}
