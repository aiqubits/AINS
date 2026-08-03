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
    MemoryNamespace, Metric, VECTOR_MAX_ENTRIES, VectorIndex, VectorIndexConfig,
    cosine_similarity_i8, quantize_i8, vector_from_value,
};

/// 缓存格式版本号；不一致时缓存视为失效。
///
/// 注意：缓存**仅存元数据**（版本 / 维度 / 度量，见 [`HnswCacheMeta`]），
/// 图本体每次从 `embeddings`（SoT）重建——故表示层变更（如 Phase 7.3
/// Cosine 图 f32 → int8）不要求 bump 版本（旧 meta 仍可校验通过，重建
/// 行为不变）。若未来缓存真正持久化图数据，必须 bump 本版本。
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

/// int8 余弦距离（Phase 7.3）：向 hnsw_rs 提供 i8 向量的距离 = 1 - 余弦。
/// 余弦尺度不变，量化向量直接入图（图内存降至 f32 的 1/4）。
struct DistCosineI8;

impl Distance<i8> for DistCosineI8 {
    fn eval(&self, va: &[i8], vb: &[i8]) -> f32 {
        // 距离 = 1 - 余弦，钳到 [0, ∞)：余弦 f32 累加在 cos≈1（如自距离）时
        // 可能因舍入略 >1 → 1-cos 略负，触发 hnsw_rs 的 `dist >= 0` 断言。
        (1.0 - cosine_similarity_i8(va, vb)).max(0.0)
    }
}

/// Cosine 用 int8 量化图（RAM 降 4x）；Euclidean 保留 f32（尺度敏感，
/// 默认无 namespace 使用）。
enum HnswBackend {
    Cosine(Hnsw<'static, i8, DistCosineI8>),
    L2(Hnsw<'static, f32, DistL2>),
}

impl HnswBackend {
    fn new(config: &VectorIndexConfig) -> Self {
        // hnsw_rs 对 `m > 256` 直接 `std::process::exit(1)`（库内硬限制）；
        // 配置来自宿主，进程不应被第三方库终止。钳制到库上限并告警：
        // 默认配置 m ≤ 32 不受影响，越界配置降级为可用上限而非静默崩溃。
        let m = config.m.clamp(4, 256) as usize;
        if config.m > 256 {
            tracing::warn!(
                m = config.m,
                "hnsw_rs caps max connections at 256; clamping index parameter"
            );
        }
        let ef_c = config.ef.max(16) as usize;
        match config.distance_metric {
            Metric::Cosine => Self::Cosine(Hnsw::new(
                m,
                VECTOR_MAX_ENTRIES,
                MAX_LAYER,
                ef_c,
                DistCosineI8,
            )),
            Metric::Euclidean => {
                Self::L2(Hnsw::new(m, VECTOR_MAX_ENTRIES, MAX_LAYER, ef_c, DistL2 {}))
            }
        }
    }

    fn insert(&self, vector: &[f32], internal_id: usize) {
        match self {
            // Cosine：量化为 i8 后入图（hnsw_rs 内部拷贝，临时量化向量可丢）
            Self::Cosine(h) => {
                let q = quantize_i8(vector);
                h.insert_slice((q.data.as_slice(), internal_id));
            }
            Self::L2(h) => h.insert_slice((vector, internal_id)),
        }
    }

    fn search(&self, query: &[f32], knn: usize, ef: usize) -> Vec<Neighbour> {
        match self {
            Self::Cosine(h) => {
                let q = quantize_i8(query);
                h.search(q.data.as_slice(), knn, ef)
            }
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
    /// 物理槽位上限（默认 `VECTOR_MAX_ENTRIES`；测试可缩小以触发槽位耗尽）。
    max_slots: usize,
    /// HNSW 的物理槽位被墓碑耗尽后置位；管理器消费该信号并从 SoT 重建。
    rebuild_required: bool,
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
            max_slots: VECTOR_MAX_ENTRIES,
            rebuild_required: false,
        }
    }

    /// 自定义活跃容量上限（测试与嵌入式小容量场景；物理槽位仍受
    /// `VECTOR_MAX_ENTRIES` 限制）。
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self
    }

    /// 自定义物理槽位上限（仅测试：无需构造 10 万节点即可触发槽位耗尽分支）。
    #[cfg(test)]
    pub fn with_max_slots(mut self, max_slots: usize) -> Self {
        self.max_slots = max_slots.max(1);
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

    /// 物理槽位是否已饱和：槽位全占且**无墓碑可回收**。饱和时新增/更新
    /// 在物理上都必须插入新节点（hnsw_rs 无真更新，同 id 更新也墓碑旧节点
    /// 并插入新节点），重建也无法回收任何槽位——管理器应确定性拒绝而非
    /// 走「写→重建→仍满→写」的 O(N) 每写全量重建循环（review 修复）。
    pub(crate) fn is_physically_saturated(&self) -> bool {
        self.tombstones.is_empty() && self.ids.len() >= self.max_slots
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
        // 内部槽位（含墓碑）触顶：hnsw_rs 图不支持删除，墓碑不释放槽位。
        // 向管理器发出明确重建信号；它会从已落盘的 embeddings（SoT）重建，
        // 不把这一派生缓存上限暴露成“必须重启”的写入失败。
        // **更新与新增同口径**：同 id 更新在物理上仍会插入一个新节点（旧
        // 节点仅墓碑化、不释放槽位），去重刷新负载下 ids/HNSW 图会持续
        // 增长；若豁免本检查，槽位满后更新永不触发重建，
        // `VECTOR_MAX_ENTRIES` 的内存上限设计被击穿。槽位满时更新同样
        // 触发重建（重建后墓碑清零、槽位回收，去重刷新可继续，不永久失败）。
        if self.ids.len() >= self.max_slots {
            self.rebuild_required = true;
            return Err(MemoryError::Storage(format!(
                "vector index internal slots exhausted ({}); rebuild required to reclaim tombstoned slots",
                self.max_slots
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
        Self::load_impl(
            namespace,
            config,
            embeddings,
            hnsw_cache,
            VECTOR_MAX_ENTRIES,
        )
        .await
    }

    /// 加载主体：`max_slots` 可注入（测试用小值触发槽位耗尽跳过路径，
    /// 无需构造 10 万节点）。
    async fn load_impl(
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
        embeddings: &dyn KvStore,
        hnsw_cache: &dyn KvStore,
        max_slots: usize,
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
        index.max_slots = max_slots.max(1);
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
        // 加载完成后索引已与 SoT 一致：清除加载期间因槽位耗尽跳过行而残留
        // 的重建信号（review 修复——标志应精确反映“load 后是否需要重建”，
        // 答案是无需；运行期 add 失败会重新置位）。
        index.rebuild_required = false;
        Ok(index)
    }
}

#[async_trait::async_trait]
impl VectorIndex for HnswVectorIndex {
    async fn add(&mut self, node_id: &str, vector: &[f32]) -> Result<(), MemoryError> {
        self.insert_internal(node_id, vector)
    }

    fn take_rebuild_required(&mut self) -> bool {
        std::mem::take(&mut self.rebuild_required)
    }

    fn is_physically_saturated(&self) -> bool {
        HnswVectorIndex::is_physically_saturated(self)
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
        // 检索结果不可能多于活跃条目；先收紧请求量，避免调用方传入极大
        // top_k 时触发 usize 加法溢出或无意义的大容量预分配。请求量再补偿
        // 墓碑数，过滤后仍能凑满实际可返回的结果数。
        let requested = top_k.min(self.len());
        let knn = requested
            .saturating_add(self.tombstones.len())
            .min(self.ids.len());
        let ef = (self.config.ef as usize).max(knn);
        let mut results = Vec::with_capacity(requested);
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
            if results.len() >= requested {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_slot_exhaustion_requests_a_rebuild_once() {
        let config = VectorIndexConfig {
            dimension: 2,
            distance_metric: Metric::Cosine,
            m: 16,
            ef: 50,
        };
        let mut index = HnswVectorIndex::new(MemoryNamespace::Personal, config);
        // Avoid constructing a 100k-node HNSW in a unit test: the guard is
        // driven by ids' physical-slot count before the backend insertion.
        index.ids = vec!["tombstone".to_string(); VECTOR_MAX_ENTRIES];
        index.max_entries = usize::MAX;

        assert!(index.insert_internal("new", &[1.0, 0.0]).is_err());
        assert!(index.take_rebuild_required());
        assert!(!index.take_rebuild_required());
    }

    #[test]
    fn physical_slot_exhaustion_triggers_rebuild_for_update_too() {
        // 回归（review 修复）：同 id 更新在物理上仍占用一个新槽位（旧节点
        // 仅墓碑化），槽位满时更新必须与新增同口径——置位重建信号并返回
        // Err。修复前更新豁免槽位检查，去重刷新负载下 ids/HNSW 图无界
        // 增长、`VECTOR_MAX_ENTRIES` 内存上限被击穿。重建后墓碑清零、
        // 槽位回收，去重刷新可继续（不永久失败）。
        let config = VectorIndexConfig {
            dimension: 2,
            distance_metric: Metric::Cosine,
            m: 16,
            ef: 50,
        };
        let mut index =
            HnswVectorIndex::new(MemoryNamespace::Personal, config.clone()).with_max_slots(1);
        index.insert_internal("first", &[1.0, 0.0]).unwrap();
        // 槽位满 + 更新已有 id：必须失败并请求重建（不再是 Ok）。
        assert!(index.insert_internal("first", &[0.5, 0.5]).is_err());
        assert!(index.take_rebuild_required());

        // 重建等价语义：槽位回收（ids 收缩为活跃数）后更新恢复成功。
        let mut recovered =
            HnswVectorIndex::new(MemoryNamespace::Personal, config).with_max_slots(2);
        recovered.insert_internal("first", &[1.0, 0.0]).unwrap();
        recovered.insert_internal("first", &[0.5, 0.5]).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(!recovered.take_rebuild_required());
    }

    #[tokio::test]
    async fn physical_saturation_is_only_when_no_tombstones_remain() {
        // review 修复回归：物理饱和 = 槽位全占**且无墓碑可回收**。有墓碑时
        // 重建可回收槽位（管理器应走重建自愈）；无墓碑时重建无益（管理器
        // 确定性拒绝，避免 O(N) 每写重建循环）。
        let config = VectorIndexConfig {
            dimension: 2,
            distance_metric: Metric::Cosine,
            m: 16,
            ef: 50,
        };
        let mut index =
            HnswVectorIndex::new(MemoryNamespace::Personal, config.clone()).with_max_slots(2);
        // 空索引未饱和。
        assert!(!index.is_physically_saturated());
        // 填满（无墓碑）→ 饱和。
        index.insert_internal("a", &[1.0, 0.0]).unwrap();
        index.insert_internal("b", &[0.0, 1.0]).unwrap();
        assert!(index.is_physically_saturated());
        // 移除一条（墓碑化）→ 不再饱和（重建可回收槽位）。
        index.remove("a").await.unwrap();
        assert!(!index.is_physically_saturated());
        // 未满但无墓碑 → 不饱和。
        let mut partial = HnswVectorIndex::new(MemoryNamespace::Personal, config).with_max_slots(4);
        partial.insert_internal("a", &[1.0, 0.0]).unwrap();
        assert!(!partial.is_physically_saturated());
    }

    #[tokio::test]
    async fn oversize_connection_parameter_is_clamped_not_process_exit() {
        // review 修复回归：hnsw_rs 对 m > 256 直接 std::process::exit(1)
        // （库内硬限制）。配置来自宿主，进程不得被第三方库终止；必须钳制
        // 到库上限而不是崩溃（默认配置 m ≤ 32 不受影响）。
        let config = VectorIndexConfig {
            dimension: 2,
            distance_metric: Metric::Cosine,
            m: 512,
            ef: 50,
        };
        let mut index = HnswVectorIndex::new(MemoryNamespace::Personal, config);
        // 钳制后索引仍可用（若未钳制，构造时进程已 exit(1)，本断言不可达）。
        index.add("ok", &[1.0, 0.0]).await.unwrap();
    }

    #[tokio::test]
    async fn oversized_top_k_is_clamped_to_available_entries() {
        let config = VectorIndexConfig {
            dimension: 2,
            distance_metric: Metric::Cosine,
            m: 16,
            ef: 50,
        };
        let mut index = HnswVectorIndex::new(MemoryNamespace::Personal, config);
        index.add("only", &[1.0, 0.0]).await.unwrap();

        let hits = index.search(&[1.0, 0.0], usize::MAX).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "only");
    }

    #[tokio::test]
    async fn load_resets_stale_rebuild_signal_after_skipping_overflow_rows() {
        // review 修复回归：SoT 行数超过物理槽位时，load 跳过超限行期间
        // 内部会置位重建信号；load 完成后该信号必须复位（索引已与 SoT
        // 一致，无需重建；运行期 add 失败会重新置位）。
        use std::sync::Arc;

        let kv = Arc::new(MockKv::new());
        let namespace = MemoryNamespace::Personal;
        for i in 0..3 {
            kv.set(
                &namespace.storage_key(&format!("row-{i}")),
                &crate::memory::vector::vector_to_value(&[1.0, 0.0]),
                None,
            )
            .await
            .unwrap();
        }
        let config = VectorIndexConfig {
            dimension: 2,
            distance_metric: Metric::Cosine,
            m: 16,
            ef: 50,
        };
        let mut loaded = HnswVectorIndex::load_impl(namespace, config, &*kv, &*kv, 2)
            .await
            .unwrap();
        assert_eq!(loaded.len(), 2, "超限行被跳过");
        assert!(!loaded.take_rebuild_required(), "load 后不得残留重建信号");
    }

    /// 微型 KV（本模块 load 契约测试用；生产路径使用 redb 后端）。
    struct MockKv(std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>);

    impl MockKv {
        fn new() -> Self {
            Self(std::sync::Mutex::new(std::collections::HashMap::new()))
        }
    }

    #[async_trait::async_trait]
    impl KvStore for MockKv {
        async fn get(&self, key: &str) -> Result<Option<serde_json::Value>, MemoryError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        async fn set(
            &self,
            key: &str,
            value: &serde_json::Value,
            _ttl: Option<std::time::Duration>,
        ) -> Result<(), MemoryError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, _key: &str) -> Result<(), MemoryError> {
            self.0.lock().unwrap().remove(_key);
            Ok(())
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
    }
}
