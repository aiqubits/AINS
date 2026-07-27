//! Model Client：Agent 与 AI Gateway 之间的纯传输层（对齐 OpenHarness `api/client.py`）。
//!
//! - 对话主通道（chat / vision）为单方法流式协议，事件为 delta / complete / retry
//!   三联合类型；重试作为流内事件上报而非静默（基线 `ApiRetryEvent` 语义）。
//! - embedding / stt / tts 为非对话直连能力的类型化便捷方法（AINS 扩展，基线无对齐物）。
//! - vision 即带 Image content block 的消息，不设独立方法。
//!
//! Agent 内所有需要 AI 能力的子系统（Kernel / Memory / Perception）共享同一个
//! `ModelClient` 单例。

use serde::{Deserialize, Serialize};

use crate::error::AgentError;
use crate::kernel::messages::ConversationMessage;
use crate::marker::MaybeSendSync;
use crate::tools::ToolDef;

/// 流式事件流：Native 端要求 `Send`，WASM 端为本地流。
#[cfg(not(target_arch = "wasm32"))]
pub type EventStream<T> = futures::stream::BoxStream<'static, T>;
#[cfg(target_arch = "wasm32")]
pub type EventStream<T> = futures::stream::LocalBoxStream<'static, T>;

/// 默认输出 token 上限（对齐基线 `max_tokens: int = 4096`）。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    /// 目标模型；`None` 时由 AI Gateway 按套餐路由。
    pub model: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub system_prompt: Option<String>,
    pub max_output_tokens: u32,
    /// 工具 JSON Schema（`ToolDef`），随请求下发。
    pub tools: Vec<ToolDef>,
}

impl Default for ModelRequest {
    fn default() -> Self {
        Self {
            model: None,
            messages: Vec::new(),
            system_prompt: None,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// 流式协议三联合事件（对齐基线 ApiTextDelta / ApiMessageComplete / ApiRetry）。
#[derive(Debug, Clone)]
pub enum ModelStreamEvent {
    TextDelta {
        text: String,
    },
    Complete {
        message: ConversationMessage,
        usage: UsageSnapshot,
        stop_reason: Option<String>,
    },
    /// 可重试失败的流内上报；重试后 UI 侧文本不得重复（见对齐清单“不复刻”第 1 条）。
    Retry {
        message: String,
        attempt: u32,
        max_attempts: u32,
        delay_secs: f32,
    },
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait ModelClient: MaybeSendSync {
    /// 对话主通道（chat / vision）：单方法流式协议，逐事件产出。
    async fn stream_response(
        &self,
        request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError>;

    /// 直连能力：文本向量化（Memory 写入/检索时调用）。
    async fn embed(&self, text: &str) -> Result<Vec<f32>, AgentError>;

    /// 直连能力：语音转文字（Perception Voice 采集后调用）。
    async fn stt(&self, audio_data: &[u8]) -> Result<String, AgentError>;

    /// 直连能力：文字转语音（Agent 回复语音输出时调用）。
    async fn tts(&self, text: &str) -> Result<Vec<u8>, AgentError>;
}
