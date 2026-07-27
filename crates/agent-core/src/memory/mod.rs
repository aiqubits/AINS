//! 用户记忆系统（AINS_PLAN 第四章）：KV / Vector / Document 三层，
//! 存储后端按平台选择（Native = redb，Web = IndexedDB），实现在 Phase 2。

pub mod document;
pub mod kv;
pub mod vector;

pub use document::{DocumentChunk, DocumentMeta, DocumentStore, SearchResult};
pub use kv::KvStore;
pub use vector::{
    MemoryEntry, MemoryNamespace, Metric, VectorIndex, VectorIndexConfig, VectorIndexManager,
};
