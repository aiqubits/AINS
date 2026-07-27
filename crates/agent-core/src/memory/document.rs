//! 文档记忆抽象（用户文件 chunk）。
//!
//! 复用 Vector Memory 基础设施，但使用独立的 `Document` namespace 索引，
//! 与 Agent 的 `Personal` namespace 在索引层完全隔离；实现在 Phase 2。

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::marker::MaybeSendSync;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentMeta {
    pub id: String,
    pub name: String,
    pub chunk_count: usize,
    /// 源文件内容哈希，用于去重（见 [`DocumentStore::is_indexed`]）。
    pub source_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChunk {
    pub chunk_id: String,
    pub doc_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub chunk: DocumentChunk,
    pub doc_name: String,
    pub score: f32,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait DocumentStore: MaybeSendSync {
    /// 索引文档：解析 → 分块 → Embedding → 写入本地存储层 + Document namespace 索引。
    async fn index(&mut self, file_path: &str) -> Result<DocumentMeta, MemoryError>;

    /// 独立文档搜索入口：仅搜索 Document namespace，可限定 doc_id 范围。
    async fn search(
        &self,
        query: &str,
        top_k: usize,
        doc_ids: Option<&[String]>,
    ) -> Result<Vec<SearchResult>, MemoryError>;

    async fn list_docs(&self) -> Result<Vec<DocumentMeta>, MemoryError>;

    async fn delete(&mut self, doc_id: &str) -> Result<(), MemoryError>;

    /// 基于 source_hash 判断文件是否已索引，避免重复处理。
    async fn is_indexed(&self, source_hash: &str) -> Result<bool, MemoryError>;
}
