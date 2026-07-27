//! Kernel 模块：消息模型（Phase 0），FSM / 事件循环（Phase 1）。

pub mod messages;

pub use messages::{ContentBlock, ConversationMessage, Role};
