//! 客户端上下文管线（Phase 5.3–5.5）：分段系统提示流水线、会话持久化、
//! 上下文压缩。对齐 OpenHarness `prompts` / `services/session_storage` /
//! `services/compact`。
//!
//! 会话内的 `ContextStore`（对话历史 + tool_metadata 状态袋）位于
//! `kernel::context`；本模块聚焦系统提示装配、快照持久化与压缩降级链。

pub mod compact;
pub mod environment;
pub mod project_docs;
pub mod prompt_pipeline;
pub mod session;

pub use compact::{
    AutoCompactState, COMPACT_PROMPT, MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES, auto_compact_if_needed,
    estimate_message_tokens, estimate_tokens, get_autocompact_threshold, microcompact_messages,
    should_autocompact, split_preserving_tool_pairs,
};
pub use environment::EnvironmentInfo;
pub use project_docs::{
    MAX_CHARS_PER_PROJECT_DOC, discover_agents_md_files, load_project_instructions,
};
pub use prompt_pipeline::{
    BASE_SYSTEM_PROMPT, PromptPipelineInput, PromptSections, build_system_prompt,
    permission_mode_section, skills_section,
};
pub use session::{
    DEFAULT_LIST_LIMIT, PERSISTED_EXTRA_KEYS, SessionSaveInput, SessionSnapshot, SessionStore,
    SessionSummary, project_slug,
};
