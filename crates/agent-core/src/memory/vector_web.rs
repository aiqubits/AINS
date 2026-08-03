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
    MemoryNamespace, Metric, QuantizedVector, VECTOR_MAX_ENTRIES_WEB, VectorIndex,
    VectorIndexConfig, cosine_similarity_i8, quantize_i8, similarity_score, vector_from_value,
};

/// 内存条目的向量存储形态：Cosine 量化为 int8（RAM 1/4，尺度不变）；
/// 其它度量（Euclidean）保持无损 f32——与 Native 端对齐（review 修复：
/// 历史实现无条件量化，Euclidean 经反量化评分产生额外量化误差，
/// 与 Native `L2(f32)` 行为不对称）。
enum StoredVector {
    /// Cosine：int8 量化向量。
    Quantized(QuantizedVector),
    /// 尺度敏感度量：无损 f32。
    Raw(Vec<f32>),
}

/// 线性向量索引（单 namespace 专属实例）。内存表按度量存 int8 量化或
/// 无损 f32 向量（Phase 7.3：Cosine 较 f32 约 1/4 RAM）。
pub struct LinearVectorIndex {
    namespace: MemoryNamespace,
    config: VectorIndexConfig,
    /// 内存连续表：(node_id, 存储向量)。
    entries: Vec<(String, StoredVector)>,
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
        // 按度量选择存储形态：Cosine 量化为 int8；其它（Euclidean）无损 f32。
        let stored = match self.config.distance_metric {
            Metric::Cosine => StoredVector::Quantized(quantize_i8(vector)),
            Metric::Euclidean => StoredVector::Raw(vector.to_vec()),
        };
        if let Some(&idx) = self.lookup.get(node_id) {
            // 同 id 重复写入视为更新
            self.entries[idx].1 = stored;
            return Ok(());
        }
        if self.len() >= VECTOR_MAX_ENTRIES_WEB {
            return Err(MemoryError::Storage(format!(
                "vector index capacity exceeded ({VECTOR_MAX_ENTRIES_WEB})"
            )));
        }
        self.lookup.insert(node_id.to_string(), self.entries.len());
        self.entries.push((node_id.to_string(), stored));
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
        // Cosine 在 i8 空间评分（尺度不变，无需反量化）；其它度量在
        // f32 空间评分（存储即无损 f32，与 Native 端一致）。
        let quantized_query = match self.config.distance_metric {
            Metric::Cosine => Some(quantize_i8(query)),
            Metric::Euclidean => None,
        };
        let mut scored: Vec<(String, f32)> = self
            .lookup
            .iter()
            .map(|(node_id, &idx)| {
                let score = match &self.entries[idx].1 {
                    StoredVector::Quantized(q) => {
                        debug_assert_eq!(self.config.distance_metric, Metric::Cosine);
                        cosine_similarity_i8(
                            &quantized_query
                                .as_ref()
                                .expect("quantized query for cosine")
                                .data,
                            &q.data,
                        )
                    }
                    StoredVector::Raw(v) => similarity_score(self.config.distance_metric, query, v),
                };
                (node_id.clone(), score)
            })
            .collect();
        // `total_cmp` 而非 `partial_cmp`：分数经 ensure_finite 保证无 NaN，
        // 但 total_cmp 让排序确定性不依赖 NaN 边界语义（防御性）。
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
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

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use super::*;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    fn config(metric: Metric) -> VectorIndexConfig {
        VectorIndexConfig {
            dimension: 3,
            distance_metric: metric,
            m: 16,
            ef: 50,
        }
    }

    #[wasm_bindgen_test]
    async fn euclidean_keeps_lossless_f32_scoring() {
        // N2 回归：Euclidean 必须存无损 f32（与 Native L2 对齐），
        // 不得量化后反量化（引入额外量化误差）。
        let mut idx = LinearVectorIndex::new(MemoryNamespace::Personal, config(Metric::Euclidean));
        let v1 = [1.0f32, 0.0, 0.0];
        let v2 = [0.99f32, 0.01, 0.0];
        idx.add("a", &v1).await.unwrap();
        idx.add("b", &v2).await.unwrap();
        let hits = idx.search(&v1, 2).await.unwrap();
        assert_eq!(hits[0].0, "a");
        // 无损评分：与直接 f32 计算逐位一致（无量化误差）。
        let direct = similarity_score(Metric::Euclidean, &v1, &v2);
        assert_eq!(hits[1].1, direct);
    }

    #[wasm_bindgen_test]
    async fn cosine_quantizes_and_scores_in_i8_space() {
        let mut idx = LinearVectorIndex::new(MemoryNamespace::Personal, config(Metric::Cosine));
        idx.add("a", &[1.0, 0.0, 0.0]).await.unwrap();
        idx.add("b", &[0.0, 1.0, 0.0]).await.unwrap();
        let hits = idx.search(&[1.0, 0.0, 0.0], 2).await.unwrap();
        assert_eq!(hits[0].0, "a");
        assert!((hits[0].1 - 1.0).abs() < 1e-4, "{}", hits[0].1);
        assert_eq!(hits[1].0, "b");
    }
}
