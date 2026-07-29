//! Web 端向量索引：纯 Rust 精确线性余弦 Top-K（AINS_PLAN 4.2）。
//!
//! `hnsw_rs` 不编译到 wasm32。启动/首次查询时从 IndexedDB `embeddings` 表把
//! 该 namespace 的向量加载为内存表，检索为精确暴力扫描（O(N)，召回 100%）；
//! 写入为 write-through（存储层 `embeddings` 由上层先落盘，本索引仅追加内存
//! 表）；容量由 `VECTOR_MAX_ENTRIES_WEB` 严格限制；无图结构、无 hnsw_cache，
//! `save` 为 no-op。

#![cfg(target_arch = "wasm32")]

use std::collections::HashMap;

use crate::error::MemoryError;
use crate::memory::kv::KvStore;
use crate::memory::vector::{
    MemoryNamespace, VECTOR_MAX_ENTRIES_WEB, VectorIndex, VectorIndexConfig, similarity_score,
    vector_from_value,
};

/// 线性向量索引（单 namespace 专属实例）。
pub struct LinearVectorIndex {
    namespace: MemoryNamespace,
    config: VectorIndexConfig,
    /// 内存连续表：(node_id, vector)。
    entries: Vec<(String, Vec<f32>)>,
    lookup: HashMap<String, usize>,
}

impl LinearVectorIndex {
    pub fn new(namespace: MemoryNamespace, config: VectorIndexConfig) -> Self {
        Self {
            namespace,
            config,
            entries: Vec::new(),
            lookup: HashMap::new(),
        }
    }

    pub fn namespace(&self) -> MemoryNamespace {
        self.namespace
    }

    pub fn len(&self) -> usize {
        self.lookup.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lookup.is_empty()
    }

    fn insert_internal(&mut self, node_id: &str, vector: &[f32]) -> Result<(), MemoryError> {
        if vector.len() != self.config.dimension as usize {
            return Err(MemoryError::Storage(format!(
                "dimension mismatch: expected {}, got {}",
                self.config.dimension,
                vector.len()
            )));
        }
        if let Some(&idx) = self.lookup.get(node_id) {
            // 同 id 重复写入视为更新
            self.entries[idx].1 = vector.to_vec();
            return Ok(());
        }
        if self.len() >= VECTOR_MAX_ENTRIES_WEB {
            return Err(MemoryError::Storage(format!(
                "vector index capacity exceeded ({VECTOR_MAX_ENTRIES_WEB})"
            )));
        }
        self.lookup.insert(node_id.to_string(), self.entries.len());
        self.entries.push((node_id.to_string(), vector.to_vec()));
        Ok(())
    }

    /// 启动加载：从 IndexedDB `embeddings` 表整表加载该 namespace 的向量。
    pub async fn load(
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
        embeddings: &dyn KvStore,
    ) -> Result<Self, MemoryError> {
        let mut index = Self::new(namespace, config);
        let prefix = namespace.storage_prefix();
        for key in embeddings.list_prefix(&prefix).await? {
            let Some(node_id) = key.strip_prefix(&prefix) else {
                continue;
            };
            let Some(value) = embeddings.get(&key).await? else {
                continue;
            };
            // 单行损坏（无法解码/维度不符/容量超限）跳过，不拖垮整个索引加载。
            let Ok(vector) = vector_from_value(&value) else {
                tracing::warn!(
                    key,
                    "skipping undecodable embedding row during linear index load"
                );
                continue;
            };
            if let Err(e) = index.insert_internal(node_id, &vector) {
                tracing::warn!(key, error = %e, "skipping embedding row during linear index load");
            }
        }
        Ok(index)
    }
}

#[async_trait::async_trait(?Send)]
impl VectorIndex for LinearVectorIndex {
    async fn add(&mut self, node_id: &str, vector: &[f32]) -> Result<(), MemoryError> {
        self.insert_internal(node_id, vector)
    }

    async fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<(String, f32)>, MemoryError> {
        if top_k == 0 || self.is_empty() {
            return Ok(Vec::new());
        }
        if query.len() != self.config.dimension as usize {
            return Err(MemoryError::Storage(format!(
                "dimension mismatch: expected {}, got {}",
                self.config.dimension,
                query.len()
            )));
        }
        let mut scored: Vec<(String, f32)> = self
            .lookup
            .iter()
            .map(|(node_id, &idx)| {
                (
                    node_id.clone(),
                    similarity_score(self.config.distance_metric, query, &self.entries[idx].1),
                )
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        Ok(scored)
    }

    async fn remove(&mut self, node_id: &str) -> Result<(), MemoryError> {
        match self.lookup.remove(node_id) {
            Some(idx) => {
                // swap_remove 压缩内存表；被换入的尾部元素需要更新 lookup 索引。
                self.entries.swap_remove(idx);
                if let Some((moved_id, _)) = self.entries.get(idx) {
                    self.lookup.insert(moved_id.clone(), idx);
                }
                Ok(())
            }
            None => Err(MemoryError::NotFound(node_id.to_string())),
        }
    }

    /// no-op：write-through 已保证 `embeddings` 落盘，无派生缓存。
    async fn save(&self, _kv: &dyn KvStore) -> Result<(), MemoryError> {
        Ok(())
    }
}
