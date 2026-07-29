//! Native 端向量索引：hnsw_rs HNSW 近似检索（AINS_PLAN 4.2）。
//!
//! - `embeddings` 表是向量的唯一事实来源（Source Of Truth）；运行时在内存中
//!   持有向量与图结构。
//! - `hnsw_cache` 为派生缓存，仅写入 [`HnswCacheMeta`]（版本/维度/度量校验元
//!   数据）。hnsw_rs 未公开图拓扑的可序列化访问接口（file_dump 仅支持文件系
//!   统，与 KvStore 抽象及 Web 对等性冲突），因此启动统一走「从 embeddings
//!   完整重建」路径——即计划中缓存失效时的兜底路径；个人记忆规模
//!   （≤ VECTOR_MAX_ENTRIES）下重建为亚秒级。偏差记录见
//!   docs/alignment/phase2-embedded-memory.md。
//! - hnsw_rs 不支持图内删除：`remove` 以墓碑（tombstone）屏蔽检索结果；
//!   重启重建后墓碑自然消失（embeddings 中对应行已被删除）。

#![cfg(not(target_arch = "wasm32"))]

use std::collections::{HashMap, HashSet};

use hnsw_rs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::memory::kv::{KvStore, now_ms};
use crate::memory::vector::{
    MemoryNamespace, Metric, VECTOR_MAX_ENTRIES, VectorIndex, VectorIndexConfig, vector_from_value,
};

/// 缓存格式版本号；不一致时缓存视为失效。
pub const HNSW_CACHE_VERSION: &str = "ains-hnsw-cache-v1";

/// HNSW 派生缓存元数据（仅 Native，AINS_PLAN 4.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswCacheMeta {
    /// 缓存格式版本号。
    pub version: String,
    /// 向量维度。
    pub dimension: u32,
    /// 距离度量（如 "cosine", "euclidean"）。
    pub metric: String,
    /// 缓存创建时间戳（Unix 毫秒）。
    pub created_at: i64,
}

fn metric_name(metric: Metric) -> &'static str {
    match metric {
        Metric::Cosine => "cosine",
        Metric::Euclidean => "euclidean",
    }
}

fn cache_key(namespace: MemoryNamespace) -> String {
    format!("hnsw/{}", namespace.as_str())
}

/// 图层数固定为 1（扁平 NSW）：hnsw_rs 的点只挂在其随机顶层，不复制到
/// 下层，上层点也不会被织入 layer-0 邻接表，导致 layer-0 检索存在概率性
/// 召回缺失（实测双点索引 ~6% 漏点、2000 点自查 ~0.5% 漏点；单层配置均为
/// 0，耗时无差异）。个人记忆规模（≤ VECTOR_MAX_ENTRIES）下单层图足够。
const MAX_LAYER: usize = 1;

enum HnswBackend {
    Cosine(Hnsw<'static, f32, DistCosine>),
    L2(Hnsw<'static, f32, DistL2>),
}

impl HnswBackend {
    fn new(config: &VectorIndexConfig) -> Self {
        let m = config.m.max(4) as usize;
        let ef_c = config.ef.max(16) as usize;
        match config.distance_metric {
            Metric::Cosine => Self::Cosine(Hnsw::new(
                m,
                VECTOR_MAX_ENTRIES,
                MAX_LAYER,
                ef_c,
                DistCosine {},
            )),
            Metric::Euclidean => {
                Self::L2(Hnsw::new(m, VECTOR_MAX_ENTRIES, MAX_LAYER, ef_c, DistL2 {}))
            }
        }
    }

    fn insert(&self, vector: &[f32], internal_id: usize) {
        match self {
            Self::Cosine(h) => h.insert_slice((vector, internal_id)),
            Self::L2(h) => h.insert_slice((vector, internal_id)),
        }
    }

    fn search(&self, query: &[f32], knn: usize, ef: usize) -> Vec<Neighbour> {
        match self {
            Self::Cosine(h) => h.search(query, knn, ef),
            Self::L2(h) => h.search(query, knn, ef),
        }
    }

    /// hnsw_rs 距离 → 统一分数（越大越相近）：cosine 距离 = 1 - 相似度。
    fn score(&self, distance: f32) -> f32 {
        match self {
            Self::Cosine(_) => 1.0 - distance,
            Self::L2(_) => -distance,
        }
    }
}

/// hnsw_rs HNSW 向量索引（单 namespace 专属实例）。
pub struct HnswVectorIndex {
    namespace: MemoryNamespace,
    config: VectorIndexConfig,
    backend: HnswBackend,
    /// 内部自增 id → node_id（hnsw_rs DataId 为 usize）。
    ids: Vec<String>,
    lookup: HashMap<String, usize>,
    tombstones: HashSet<usize>,
    /// 活跃条目容量上限（默认 `VECTOR_MAX_ENTRIES`）。
    max_entries: usize,
}

impl HnswVectorIndex {
    pub fn new(namespace: MemoryNamespace, config: VectorIndexConfig) -> Self {
        let backend = HnswBackend::new(&config);
        Self {
            namespace,
            config,
            backend,
            ids: Vec::new(),
            lookup: HashMap::new(),
            tombstones: HashSet::new(),
            max_entries: VECTOR_MAX_ENTRIES,
        }
    }

    /// 自定义活跃容量上限（测试与嵌入式小容量场景；物理槽位仍受
    /// `VECTOR_MAX_ENTRIES` 限制）。
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self
    }

    pub fn namespace(&self) -> MemoryNamespace {
        self.namespace
    }

    /// 活跃（非墓碑）条目数。
    pub fn len(&self) -> usize {
        self.ids.len() - self.tombstones.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn insert_internal(&mut self, node_id: &str, vector: &[f32]) -> Result<(), MemoryError> {
        if vector.len() != self.config.dimension as usize {
            return Err(MemoryError::Storage(format!(
                "dimension mismatch: expected {}, got {}",
                self.config.dimension,
                vector.len()
            )));
        }
        // 同 id 更新不改变活跃条目数，不受活跃容量限制（与 Web 端同语义，
        // 否则去重刷新在容量满时永久失败）
        let updating = self.lookup.contains_key(node_id);
        if !updating && self.len() >= self.max_entries {
            return Err(MemoryError::Storage(format!(
                "vector index capacity exceeded ({})",
                self.max_entries
            )));
        }
        // 内部槽位（含墓碑）触顶：hnsw_rs 图不支持删除，墓碑不释放槽位，
        // 重启从 embeddings 重建后墓碑消失即可恢复。
        if self.ids.len() >= VECTOR_MAX_ENTRIES {
            return Err(MemoryError::Storage(format!(
                "vector index internal slots exhausted ({VECTOR_MAX_ENTRIES}); restart to rebuild and reclaim tombstoned slots"
            )));
        }
        // 同 id 重复写入视为更新：墓碑旧节点，插入新节点
        if let Some(old) = self.lookup.get(node_id).copied() {
            self.tombstones.insert(old);
        }
        let internal_id = self.ids.len();
        self.backend.insert(vector, internal_id);
        self.ids.push(node_id.to_string());
        self.lookup.insert(node_id.to_string(), internal_id);
        Ok(())
    }

    /// 启动加载：从 `embeddings`（Source Of Truth）重建图，并校验 / 刷新
    /// `hnsw_cache` 元数据。
    pub async fn load(
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
        embeddings: &dyn KvStore,
        hnsw_cache: &dyn KvStore,
    ) -> Result<Self, MemoryError> {
        // 校验派生缓存元数据；失效（缺失/版本或维度不匹配）不影响数据完整性
        let cache_valid = match hnsw_cache.get(&cache_key(namespace)).await? {
            Some(raw) => serde_json::from_value::<HnswCacheMeta>(raw).is_ok_and(|meta| {
                meta.version == HNSW_CACHE_VERSION
                    && meta.dimension == config.dimension
                    && meta.metric == metric_name(config.distance_metric)
            }),
            None => false,
        };

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
                tracing::warn!(key, "skipping undecodable embedding row during HNSW load");
                continue;
            };
            if let Err(e) = index.insert_internal(node_id, &vector) {
                tracing::warn!(key, error = %e, "skipping embedding row during HNSW load");
            }
        }

        if !cache_valid {
            index.save(hnsw_cache).await?;
        }
        Ok(index)
    }
}

#[async_trait::async_trait]
impl VectorIndex for HnswVectorIndex {
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
        // 请求量补偿墓碑数，过滤后仍能凑满 top_k
        let knn = (top_k + self.tombstones.len()).min(self.ids.len());
        let ef = (self.config.ef as usize).max(knn);
        let mut results = Vec::with_capacity(top_k);
        let mut seen = HashSet::new();
        for neighbour in self.backend.search(query, knn, ef) {
            let internal_id = neighbour.d_id;
            if self.tombstones.contains(&internal_id) {
                continue;
            }
            let Some(node_id) = self.ids.get(internal_id) else {
                continue;
            };
            // 同 id 更新后旧节点已墓碑化，这里再做一次去重防御
            if !seen.insert(node_id.clone()) {
                continue;
            }
            results.push((node_id.clone(), self.backend.score(neighbour.distance)));
            if results.len() >= top_k {
                break;
            }
        }
        Ok(results)
    }

    async fn remove(&mut self, node_id: &str) -> Result<(), MemoryError> {
        match self.lookup.remove(node_id) {
            Some(internal_id) => {
                self.tombstones.insert(internal_id);
                Ok(())
            }
            None => Err(MemoryError::NotFound(node_id.to_string())),
        }
    }

    async fn save(&self, kv: &dyn KvStore) -> Result<(), MemoryError> {
        let meta = HnswCacheMeta {
            version: HNSW_CACHE_VERSION.to_string(),
            dimension: self.config.dimension,
            metric: metric_name(self.config.distance_metric).to_string(),
            created_at: now_ms(),
        };
        let value =
            serde_json::to_value(&meta).map_err(|e| MemoryError::Serialization(e.to_string()))?;
        kv.set(&cache_key(self.namespace), &value, None).await
    }
}
