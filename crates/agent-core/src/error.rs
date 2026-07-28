//! 核心错误类型。

use thiserror::Error;

use crate::memory::MemoryNamespace;

/// 记忆系统错误（KV / Vector / Document 三层共用）。
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("vector index namespace not found: {0:?}")]
    NamespaceNotFound(MemoryNamespace),
    #[error("entry not found: {0}")]
    NotFound(String),
}

/// 工具运行时错误。
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),
    #[error("invalid tool input: {0}")]
    InvalidInput(String),
    #[error("tool execution failed: {0}")]
    Execution(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
}

/// Skills 系统错误。
#[derive(Debug, Error)]
pub enum SkillsError {
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("invalid skill format: {0}")]
    InvalidFormat(String),
    #[error("skill storage error: {0}")]
    Storage(String),
}

/// Agent Runtime 顶层错误。
#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Skills(#[from] SkillsError),
    #[error("model transport error: {0}")]
    Model(String),
    #[error("max turns exceeded")]
    /// 预留变体：当前实现中 MaxTurnsExceeded 回 Idle（会话可继续），
    /// 不使用此变体。Phase 2 若引入严格轮次限制可从此处返回 `Failed`。
    MaxTurnsExceeded,
}
