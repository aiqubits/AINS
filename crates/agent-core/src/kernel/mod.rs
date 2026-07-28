//! Kernel 模块：事件驱动状态机核心（FSM + 事件循环 + 消息模型 + 上下文）。

pub mod context;
pub mod event_loop;
pub mod fsm;
pub mod messages;
#[cfg(not(target_arch = "wasm32"))]
pub mod mock_model;
pub mod state;
pub mod stream_events;

pub use context::ContextStore;
pub use event_loop::{AgentKernel, AgentKernelConfig};
pub use messages::{
    ContentBlock, ConversationMessage, Role, ToolUse, sanitize_conversation_messages,
};
#[cfg(not(target_arch = "wasm32"))]
pub use mock_model::ScriptedModelClient;
pub use state::{AgentEvent, AgentState, Attachment, CompactTrigger, StateKind, SystemEventType};
pub use stream_events::StreamEvent;
