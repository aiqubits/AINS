//! 异步 system prompt provider（AINS 向量表生产路径调用方设计 §12）。
//!
//! Kernel `Querying` 在构造 ModelRequest 前 await provider，动态注入
//! memory section（base system prompt + dynamic memory + permission mode
//! 三段的最终拼装顺序见 event_loop.rs）。provider 失败/无内容返回 `None`，
//! Kernel 回落 base system prompt + permission mode section——Memory 失败
//! 不阻断主 Agent（§12.2）。

use crate::kernel::messages::ConversationMessage;
use crate::marker::MaybeSendSync;

/// 动态 system prompt provider：为每个 Querying 轮提供额外的 system prompt
/// 段（如 scoped memory recall）。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait AsyncSystemPromptProvider: MaybeSendSync {
    /// 返回追加的 system prompt 段；`None` 表示无内容或失败（回落原提示）。
    async fn provide(&self, messages: &[ConversationMessage]) -> Option<String>;
}
