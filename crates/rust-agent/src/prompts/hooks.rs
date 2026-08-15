//! Prompt Hook 的结构化结果约束。

/// Prompt Hook 校验模型的 system prompt。
pub const PROMPT_HOOK_SYSTEM_PROMPT: &str = "You are validating whether a hook condition passes in AINS. \
Return strict JSON: {\"ok\": true} or {\"ok\": false, \"reason\": \"...\"}.";
