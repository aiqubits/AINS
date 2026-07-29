//! 文档记忆（用户文件 chunk）：抽象 + 本地实现。
//!
//! 复用 Vector Memory 基础设施，但使用独立的 `Document` namespace 索引，
//! 与 Agent 的 `Personal` namespace 在索引层完全隔离（AINS_PLAN 4.3）。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::MemoryError;
use crate::marker::MaybeSendSync;
use crate::memory::engine::MemoryEngine;
use crate::memory::kv::{KvStore, now_ms};
use crate::memory::parser::{DocumentKind, chunk_document};
use crate::memory::vector::MemoryNamespace;
use crate::model_client::ModelClient;

/// Embedding 批量上限（AINS_PLAN 4.3：每次最多 20 条；`ModelClient::embed`
/// 当前为单条接口，传输层批量合并在 Phase 5 远程客户端落地）。
pub const EMBED_BATCH_MAX: usize = 20;

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

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn doc_key(doc_id: &str) -> String {
    format!("doc/{doc_id}")
}

fn hash_key(source_hash: &str) -> String {
    format!("hash/{source_hash}")
}

/// 本地 DocumentStore 实现：`documents` 表存元数据，chunk 内容与向量经
/// [`MemoryEngine`] 写入 `memories` / `embeddings` + Document namespace 索引。
pub struct LocalDocumentStore {
    documents: Arc<dyn KvStore>,
    engine: MemoryEngine,
    model: Arc<dyn ModelClient>,
}

impl LocalDocumentStore {
    /// `engine` 需已创建 `MemoryNamespace::Document` 索引。
    pub fn new(
        documents: Arc<dyn KvStore>,
        engine: MemoryEngine,
        model: Arc<dyn ModelClient>,
    ) -> Self {
        Self {
            documents,
            engine,
            model,
        }
    }

    async fn get_meta(&self, doc_id: &str) -> Result<Option<DocumentMeta>, MemoryError> {
        match self.documents.get(&doc_key(doc_id)).await? {
            Some(raw) => Ok(Some(
                serde_json::from_value(raw)
                    .map_err(|e| MemoryError::Serialization(e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// best-effort 删除 doc 的前 `count` 个 chunk（`forget` 容忍不存在的行）。
    async fn cleanup_chunks(&mut self, doc_id: &str, count: usize) {
        for i in 0..count {
            let chunk_id = format!("{doc_id}-c{i}");
            let _ = self
                .engine
                .forget(MemoryNamespace::Document, &chunk_id)
                .await;
        }
    }

    /// 索引文本内容（双端可用入口；`index(file_path)` 是 Native 端的文件包装）。
    pub async fn index_content(
        &mut self,
        name: &str,
        content: &str,
    ) -> Result<DocumentMeta, MemoryError> {
        let source_hash = hex_sha256(content.as_bytes());
        // 去重：hash 行或既有 meta 损坏时视为未命中，重新索引并覆写，
        // 避免单行损坏永久阻断同内容文档入库。
        let existing = match self.documents.get(&hash_key(&source_hash)).await {
            Ok(value) => value,
            Err(MemoryError::Serialization(e)) => {
                tracing::warn!(source_hash, error = %e, "corrupt hash row; re-indexing");
                None
            }
            Err(e) => return Err(e),
        };
        if let Some(serde_json::Value::String(existing_id)) = existing {
            match self.get_meta(&existing_id).await {
                Ok(Some(meta)) => return Ok(meta),
                Ok(None) => {}
                Err(MemoryError::Serialization(e)) => {
                    tracing::warn!(doc_id = existing_id, error = %e, "corrupt doc meta; re-indexing");
                }
                Err(e) => return Err(e),
            }
        }

        let kind = DocumentKind::from_name(name);
        let chunks = chunk_document(kind, content);
        if chunks.is_empty() {
            return Err(MemoryError::Storage(format!(
                "document {name} produced no chunks"
            )));
        }

        let doc_id = format!("doc-{}-{}", now_ms(), &source_hash[..12]);
        for (i, chunk) in chunks.iter().enumerate() {
            let inserted = async {
                let vector = self
                    .model
                    .embed(chunk)
                    .await
                    .map_err(|e| MemoryError::Storage(format!("embedding failed: {e}")))?;
                let chunk_id = format!("{doc_id}-c{i}");
                self.engine
                    .insert_with_id(
                        MemoryNamespace::Document,
                        &chunk_id,
                        chunk,
                        &vector,
                        json!({ "doc_id": doc_id, "doc_name": name, "chunk_index": i }),
                    )
                    .await?;
                Ok::<(), MemoryError>(())
            }
            .await;
            if let Err(e) = inserted {
                // 中途失败：best-effort 回收已写入的 chunk（含第 i 块可能的
                // 部分写入行），避免无 meta 的孤儿数据
                self.cleanup_chunks(&doc_id, i + 1).await;
                return Err(e);
            }
        }

        let meta = DocumentMeta {
            id: doc_id.clone(),
            name: name.to_string(),
            chunk_count: chunks.len(),
            source_hash: source_hash.clone(),
        };
        let meta_written = async {
            self.documents
                .set(
                    &doc_key(&doc_id),
                    &serde_json::to_value(&meta)
                        .map_err(|e| MemoryError::Serialization(e.to_string()))?,
                    None,
                )
                .await?;
            self.documents
                .set(
                    &hash_key(&source_hash),
                    &serde_json::Value::String(doc_id.clone()),
                    None,
                )
                .await?;
            Ok::<(), MemoryError>(())
        }
        .await;
        if let Err(e) = meta_written {
            // meta/hash 写入失败：撤销可能已写入的 doc/ 行并回收全部 chunk，
            // 避免留下无法通过 list_docs 发现的孤儿数据
            let _ = self.documents.delete(&doc_key(&doc_id)).await;
            self.cleanup_chunks(&doc_id, chunks.len()).await;
            return Err(e);
        }
        Ok(meta)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl DocumentStore for LocalDocumentStore {
    async fn index(&mut self, file_path: &str) -> Result<DocumentMeta, MemoryError> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = file_path;
            Err(MemoryError::Storage(
                "file paths are unavailable on web; use index_content".into(),
            ))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let bytes = std::fs::read(file_path)
                .map_err(|e| MemoryError::Storage(format!("read {file_path}: {e}")))?;
            let name = std::path::Path::new(file_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(file_path)
                .to_string();
            let content = if DocumentKind::from_name(&name) == DocumentKind::Pdf {
                crate::memory::parser::extract_pdf_text(&bytes)?
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            self.index_content(&name, &content).await
        }
    }

    async fn search(
        &self,
        query: &str,
        top_k: usize,
        doc_ids: Option<&[String]>,
    ) -> Result<Vec<SearchResult>, MemoryError> {
        let vector = self
            .model
            .embed(query)
            .await
            .map_err(|e| MemoryError::Storage(format!("embedding failed: {e}")))?;
        // 限定 doc_id 时过采样；目标文档占 namespace 比例过小时固定倍数
        // 仍会欠采样，命中不足且索引尚有候选时按 4 倍扩窗重试，
        // 直到凑满 top_k 或 fetch_k 覆盖 namespace 全部条目。耗尽判定
        // 不能用 `hits.len() < fetch_k`：回填跳过的损坏行会使 hits 偷减，
        // 在索引尚有候选时误判耗尽而提前停止扩窗（欠取）；改用
        // namespace 总条数上界（索引节点 ⊆ memories 行，HNSW 近似召回
        // 缺失属已接受偏差）。
        let mut fetch_k = if doc_ids.is_some() {
            top_k.saturating_mul(4)
        } else {
            top_k
        };
        let total = if doc_ids.is_some() {
            self.engine.count(MemoryNamespace::Document).await?
        } else {
            0
        };
        loop {
            let hits = self
                .engine
                .search(MemoryNamespace::Document, &vector, fetch_k)
                .await?;
            let exhausted = fetch_k >= total;
            let mut results = Vec::new();
            for (entry, score) in hits {
                let doc_id = entry
                    .metadata
                    .get("doc_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if let Some(filter) = doc_ids
                    && !filter.iter().any(|d| d == &doc_id)
                {
                    continue;
                }
                let doc_name = entry
                    .metadata
                    .get("doc_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                results.push(SearchResult {
                    chunk: DocumentChunk {
                        chunk_id: entry.id,
                        doc_id,
                        content: entry.content,
                    },
                    doc_name,
                    score,
                });
                if results.len() >= top_k {
                    break;
                }
            }
            if results.len() >= top_k || doc_ids.is_none() || exhausted {
                return Ok(results);
            }
            fetch_k = fetch_k.saturating_mul(4);
        }
    }

    async fn list_docs(&self) -> Result<Vec<DocumentMeta>, MemoryError> {
        let mut docs = Vec::new();
        for key in self.documents.list_prefix("doc/").await? {
            // 单行损坏跳过，不毒化文档列表；其余存储错误照常上抛。
            let raw = match self.documents.get(&key).await {
                Ok(Some(raw)) => raw,
                Ok(None) => continue,
                Err(MemoryError::Serialization(e)) => {
                    tracing::warn!(key, error = %e, "skipping corrupt doc meta row");
                    continue;
                }
                Err(e) => return Err(e),
            };
            match serde_json::from_value::<DocumentMeta>(raw) {
                Ok(meta) => docs.push(meta),
                Err(e) => {
                    tracing::warn!(key, error = %e, "skipping undecodable doc meta row");
                }
            }
        }
        Ok(docs)
    }

    async fn delete(&mut self, doc_id: &str) -> Result<(), MemoryError> {
        // meta 行损坏（无法解码）不应阻断删除：否则该文档的 chunk /
        // hash 行永久残留占用容量（index_content 的去重路径已容忍同类损坏）。
        let meta = match self.get_meta(doc_id).await {
            Ok(Some(meta)) => Some(meta),
            Ok(None) => return Err(MemoryError::NotFound(doc_id.to_string())),
            Err(MemoryError::Serialization(e)) => {
                tracing::warn!(doc_id, error = %e, "corrupt doc meta; deleting by prefix scan");
                None
            }
            Err(e) => return Err(e),
        };
        // chunk 按实际存在的 id 前缀列举回收，不依赖可能损坏的 chunk_count
        for chunk_id in self
            .engine
            .list_ids(MemoryNamespace::Document, &format!("{doc_id}-c"))
            .await?
        {
            self.engine
                .forget(MemoryNamespace::Document, &chunk_id)
                .await?;
        }
        self.documents.delete(&doc_key(doc_id)).await?;
        match meta {
            Some(meta) => self.documents.delete(&hash_key(&meta.source_hash)).await?,
            None => {
                // 损坏 meta 拿不到 source_hash：反查指向本 doc 的 hash 映射
                for key in self.documents.list_prefix("hash/").await? {
                    let points_here = match self.documents.get(&key).await {
                        Ok(Some(serde_json::Value::String(id))) => id == doc_id,
                        Ok(_) => false,
                        Err(MemoryError::Serialization(_)) => false,
                        Err(e) => return Err(e),
                    };
                    if points_here {
                        self.documents.delete(&key).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn is_indexed(&self, source_hash: &str) -> Result<bool, MemoryError> {
        match self.documents.get(&hash_key(source_hash)).await {
            Ok(value) => Ok(value.is_some()),
            // hash 行损坏视为未索引：重新索引会覆写修复该行
            Err(MemoryError::Serialization(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}
