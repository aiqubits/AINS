//! 内置模型提示词与固定模板的单一审核入口。
//!
//! 本模块只保存固定文本及不含外部数据的纯模板。涉及不可信数据的截断、
//! 转义、限额和渲染仍归原始数据模块负责，避免集中提示词时削弱安全边界。

pub mod compaction;
pub mod core;
pub mod hooks;
pub mod memory;
pub mod perception;
pub mod tools;

pub use compaction::{COMPACT_PROMPT, COMPACTION_SYSTEM_PROMPT};
pub use core::{BASE_SYSTEM_PROMPT, permission_mode_guidance};
pub use hooks::PROMPT_HOOK_SYSTEM_PROMPT;
pub use memory::{
    EXTRACTION_SYSTEM_PROMPT, durable_memory_extraction_request, legacy_memory_extraction_request,
};
pub use perception::DEFAULT_VISION_PROMPT;
pub use tools::TOOL_CALL_PROTOCOL_HEADER;
