//! 向量记忆抽象（长期记忆 / 语义检索）。
//!
//! 每个 [`MemoryNamespace`] 拥有独立的向量索引实例（Native = hnsw_rs HNSW 近似
//! 检索，Web = 纯 Rust 精确线性余弦 Top-K），由 [`VectorIndexManager`] 聚合；
//! 记忆内容始终存储在本地存储层（`memories` / `embeddings`），索引仅通过
//! `node_id` 引用。双端实现同一行为契约，实现在 Phase 2。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::MemoryError;
use crate::marker::MaybeSendSync;
use crate::memory::kv::KvStore;

/// 记忆命名空间；每个 namespace 拥有独立向量索引，检索时互不干扰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryNamespace {
    /// Agent 自动记忆（小规模高召回）。
    Personal,
    /// 用户文件 chunk（大规模高吞吐）。
    Document,
    /// 代码知识库（结构化搜索）。
    Code,
    /// 企业知识库。
    EnterpriseKnowledge,
    /// 临时记忆（短生命周期）。
    Temporary,
}

impl MemoryNamespace {
    pub const ALL: [Self; 5] = [
        Self::Personal,
        Self::Document,
        Self::Code,
        Self::EnterpriseKnowledge,
        Self::Temporary,
    ];

    /// 存储 key 前缀用的稳定标识（与 serde snake_case 一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Document => "document",
            Self::Code => "code",
            Self::EnterpriseKnowledge => "enterprise_knowledge",
            Self::Temporary => "temporary",
        }
    }

    /// 本地存储层 key：`{namespace}/{id}`（memories / embeddings 两表共用）。
    pub fn storage_key(&self, id: &str) -> String {
        format!("{}/{id}", self.as_str())
    }

    /// 本地存储层 key 前缀：`{namespace}/`。
    pub fn storage_prefix(&self) -> String {
        format!("{}/", self.as_str())
    }
}

/// 距离度量（两平台生效，余弦计算双端共用同一函数保证分数口径一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Cosine,
    Euclidean,
}

/// 每 Namespace 独立的索引配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexConfig {
    pub dimension: u32,
    pub distance_metric: Metric,
    /// HNSW 图连接数（仅 Native/hnsw_rs 生效，Web 线性索引忽略）。
    pub m: u16,
    /// 搜索扩展因子（仅 Native/hnsw_rs 生效，Web 线性索引忽略）。
    pub ef: u16,
}

impl VectorIndexConfig {
    /// 各 namespace 的默认配置（AINS_PLAN 4.2 Namespace 配置示例）。
    pub fn default_for(namespace: MemoryNamespace) -> Self {
        let (dimension, m, ef) = match namespace {
            MemoryNamespace::Personal => (1536, 16, 50),
            MemoryNamespace::Document => (1536, 32, 100),
            MemoryNamespace::Code => (768, 24, 80),
            MemoryNamespace::EnterpriseKnowledge => (1536, 32, 100),
            MemoryNamespace::Temporary => (1536, 16, 50),
        };
        Self {
            dimension,
            distance_metric: Metric::Cosine,
            m,
            ef,
        }
    }
}

/// Native 端单 namespace 向量容量上限。
pub const VECTOR_MAX_ENTRIES: usize = 100_000;
/// Web 端单 namespace 向量容量上限（线性检索 O(N) 天花板，见附录 B）。
pub const VECTOR_MAX_ENTRIES_WEB: usize = 10_000;

/// 当前编译目标的容量上限。
pub const fn vector_max_entries() -> usize {
    #[cfg(target_arch = "wasm32")]
    {
        VECTOR_MAX_ENTRIES_WEB
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        VECTOR_MAX_ENTRIES
    }
}

/// 余弦相似度（双端共用，保证分数口径一致）；零向量相似度为 0。
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// 统一检索分数：**越大越相近**（Cosine = 相似度；Euclidean = 负距离）。
pub fn similarity_score(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::Cosine => cosine_similarity(a, b),
        Metric::Euclidean => {
            let sum: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum();
            -sum.sqrt()
        }
    }
}

// ── int8 量化（Phase 7.3 内存优化）──────────────────────────────
//
// 向量索引的内存占用主要来自驻留内存的向量（1536 维 f32 = 6KiB/条）。
// 对称每向量 int8 量化将其压至 1/4（1536 维 i8 = 1.5KiB/条），直接降低
// Native HNSW 图与 Web 线性表的 RAM。关键性质：**余弦相似度尺度不变**
// （cosine(k·a, k·b)=cosine(a,b)，k>0），故量化向量的余弦≈原余弦（仅舍入误差），
// 无需反量化即可在 i8 空间直接检索。embeddings 表仍存无损 f32（SoT），
// 量化仅作用于内存索引（重建时从 f32 量化入索引）。

/// 对称 int8 量化向量：`data[i] ≈ original[i] / scale`（scale > 0）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuantizedVector {
    /// 每向量量化尺度（max_abs / 127）。
    pub scale: f32,
    /// 量化分量（[-127, 127]）。
    pub data: Vec<i8>,
}

impl QuantizedVector {
    pub fn dim(&self) -> usize {
        self.data.len()
    }
}

/// f32 向量 → 对称 int8 量化（scale = max|v| / 127）。零向量 scale=1。
/// NaN/Inf 分量映射为 0i8 避免 UB（`NaN as i8` 在 Rust 中为未定义行为）。
pub fn quantize_i8(vector: &[f32]) -> QuantizedVector {
    let max_abs = vector.iter().fold(0.0f32, |m, &v| {
        if v.is_finite() && v.abs() > m {
            v.abs()
        } else {
            m
        }
    });
    let scale = if max_abs > 0.0 {
        let scale = max_abs / 127.0;
        // subnormal 级 max_abs（< ~1.78e-43）除以 127 会下溢为 0 → v/0 = ±Inf
        // → round/clamp 饱和为 ±127，Euclidean 反量化（dequantize_i8）严重失真。
        // 下溢时回退 1.0（分量映射为 ±1，方向保留，Cosine 尺度不变性不受影响）。
        if scale == 0.0 { 1.0 } else { scale }
    } else {
        1.0
    };
    let data = vector
        .iter()
        .map(|&v| {
            if !v.is_finite() {
                return 0i8;
            }
            (v / scale).round().clamp(-127.0, 127.0) as i8
        })
        .collect();
    QuantizedVector { scale, data }
}

/// 量化向量 → f32（近似重建，供 Euclidean 等需真实尺度的场景）。
pub fn dequantize_i8(q: &QuantizedVector) -> Vec<f32> {
    q.data.iter().map(|&d| f32::from(d) * q.scale).collect()
}

/// i8 向量余弦相似度（尺度不变，无需 scale）；i64 累加防溢出，
/// 终步经 f64 除法（1536 维 i64 平方和可超 f32 精确整数范围 2²⁴），
/// 结果裁剪到 [-1, 1]。零向量为 0。
pub fn cosine_similarity_i8(a: &[i8], b: &[i8]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "quantized vectors must share a dimension");
    let (mut dot, mut na, mut nb) = (0i64, 0i64, 0i64);
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (i64::from(*x), i64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0 || nb == 0 {
        return 0.0;
    }
    let cos = dot as f64 / ((na as f64).sqrt() * (nb as f64).sqrt());
    (cos as f32).clamp(-1.0, 1.0)
}

/// 量化空间统一检索分数（Cosine 尺度不变直算 i8；Euclidean 反量化后算）。
pub fn quantized_score(metric: Metric, query: &QuantizedVector, entry: &QuantizedVector) -> f32 {
    match metric {
        Metric::Cosine => cosine_similarity_i8(&query.data, &entry.data),
        Metric::Euclidean => similarity_score(metric, &dequantize_i8(query), &dequantize_i8(entry)),
    }
}

/// 向量 → 存储层 JSON 值（embeddings 表载荷）。
pub fn vector_to_value(vector: &[f32]) -> Value {
    Value::Array(
        vector
            .iter()
            .map(|v| {
                serde_json::Number::from_f64(f64::from(*v))
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            })
            .collect(),
    )
}

/// 存储层 JSON 值 → 向量。
pub fn vector_from_value(value: &Value) -> Result<Vec<f32>, MemoryError> {
    value
        .as_array()
        .ok_or_else(|| MemoryError::Serialization("embedding is not an array".into()))?
        .iter()
        .map(|v| {
            let value = v.as_f64().ok_or_else(|| {
                MemoryError::Serialization("embedding element not a number".into())
            })?;
            // JSON numbers are f64, while the vector index stores f32.  A
            // finite f64 outside f32's range silently casts to infinity;
            // reject it as corrupt input instead of admitting a non-finite
            // value into distance calculations during index rebuild.
            let value = value as f32;
            if value.is_finite() {
                Ok(value)
            } else {
                Err(MemoryError::Serialization(
                    "embedding element is outside the finite f32 range".into(),
                ))
            }
        })
        .collect()
}

/// 记忆内容（存储在本地存储层，Single Source of Truth）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub namespace: MemoryNamespace,
    pub metadata: Value,
    pub created_at: i64,
}

/// 单 Namespace 向量索引；实例专属一个 namespace，不接收 namespace 参数。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait VectorIndex: MaybeSendSync {
    async fn add(&mut self, node_id: &str, vector: &[f32]) -> Result<(), MemoryError>;

    /// Consume a backend-specific request to rebuild this derived index from
    /// its source of truth.  Most implementations never need this; native
    /// HNSW uses it when tombstones exhaust its fixed physical slot budget.
    /// Callers must only rebuild from an already-persisted source of truth.
    fn take_rebuild_required(&mut self) -> bool {
        false
    }

    /// 物理槽位是否已饱和（槽位全占且无墓碑可回收）。默认实现保守返回
    /// `false`（线性表后端无独立物理槽位概念）；Native HNSW 覆盖为真实
    /// 判定。管理器会拒绝饱和索引中的新增节点；已有节点的刷新则可从
    /// 已更新的 SoT 重建，回收该刷新产生的旧物理节点。
    fn is_physically_saturated(&self) -> bool {
        false
    }

    /// 当前是否包含一个活跃节点。物理槽位饱和时，管理器据此区分会增加
    /// 活跃条目的新增写入（应拒绝）与同 id 刷新（可请求从 SoT 重建）。
    fn contains_node(&self, _node_id: &str) -> bool {
        false
    }

    /// 相似度检索，返回 `(node_id, score)`，score 方向与 Metric 语义一致。
    async fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<(String, f32)>, MemoryError>;

    async fn remove(&mut self, node_id: &str) -> Result<(), MemoryError>;

    /// 持久化派生数据（Native：HNSW 图拓扑写入 `hnsw_cache`；Web：no-op，
    /// write-through 已保证 `embeddings` 落盘）。
    async fn save(&self, kv: &dyn KvStore) -> Result<(), MemoryError>;
}

/// 多 Namespace 向量索引聚合（AINS_PLAN 4.2 / 7.4 冷启动懒加载）。
///
/// 索引采用「按需物化」语义：`create_index` 仅登记 namespace 与配置，
/// **不触发任何存储 I/O 或图重建**；直到该 namespace 首次被 `search` 命中，
/// 才从 `embeddings`（Source Of Truth）重建索引（懒加载）。写入路径
/// (`add`/`remove`) 遵循「SoT 先行」契约——调用方须已把向量写入 / 删除
/// `embeddings` 之后再调用，故对尚未物化的索引写入为 no-op（下次物化时从
/// SoT 自然纳入），从而显著降低进程冷启动开销：未被检索的 namespace 永不重建。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait VectorIndexManager: MaybeSendSync {
    /// 登记 namespace 索引（懒加载：仅记录配置，不做 I/O、不重建图）。
    async fn create_index(
        &mut self,
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
    ) -> Result<(), MemoryError>;

    /// 删除指定 namespace 的向量索引（含所有节点）。实现应保证幂等
    /// （ensure-absent）：索引不存在时也删除遗留存储数据并返回 Ok。
    async fn remove_index(&mut self, namespace: MemoryNamespace) -> Result<(), MemoryError>;

    /// 向 namespace 索引写入节点。契约：调用方须已把向量落盘到 `embeddings`
    /// （SoT）；索引尚未物化时本调用为 no-op（惰性重建时从 SoT 纳入）。
    async fn add(
        &mut self,
        namespace: MemoryNamespace,
        node_id: &str,
        vector: &[f32],
    ) -> Result<(), MemoryError>;

    /// 从 namespace 索引移除节点。契约同 `add`：索引未物化时为 no-op
    /// （调用方已从 SoT 删除，惰性重建时自然不再纳入）。
    async fn remove(
        &mut self,
        namespace: MemoryNamespace,
        node_id: &str,
    ) -> Result<(), MemoryError>;

    /// 相似度检索。**首次命中该 namespace 时触发懒加载**（从 `embeddings`
    /// 重建索引），随后直接在内存索引检索，返回 `(node_id, score)`。
    async fn search(
        &self,
        namespace: MemoryNamespace,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<(String, f32)>, MemoryError>;

    /// 将指定 namespace 的派生数据落盘（Native 写 hnsw_cache；Web no-op）。
    /// 尚未物化（Pending）的索引无内存派生数据可持久化，跳过。
    /// `embeddings` 是 Source of Truth，hnsw_cache 缺失不影响正确性（§15）。
    async fn save_index(&self, _namespace: MemoryNamespace) -> Result<(), MemoryError> {
        Ok(())
    }

    /// 关闭前保存全部**已物化**索引的派生数据（默认 no-op；Native 覆盖为
    /// HNSW 落盘）。不得阻塞每次 assistant stream（§15 生命周期约定）。
    async fn save_all(&self) -> Result<(), MemoryError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_dequantize_roundtrip_within_tolerance() {
        let v = vec![0.9f32, -0.4, 0.1, 0.75, -0.02, 0.5];
        let q = quantize_i8(&v);
        assert_eq!(q.dim(), v.len());
        let back = dequantize_i8(&q);
        for (orig, approx) in v.iter().zip(back.iter()) {
            // 量化误差 ≤ scale/2 = max_abs/254 ≈ 0.0035
            assert!((orig - approx).abs() <= q.scale, "{orig} vs {approx}");
        }
    }

    #[test]
    fn i8_cosine_matches_f32_cosine_within_tolerance() {
        let a = vec![0.2f32, 0.9, -0.3, 0.5, 0.1, -0.7];
        let b = vec![0.25f32, 0.85, -0.2, 0.55, 0.05, -0.6];
        let f32_cos = cosine_similarity(&a, &b);
        let qa = quantize_i8(&a);
        let qb = quantize_i8(&b);
        let i8_cos = cosine_similarity_i8(&qa.data, &qb.data);
        assert!((f32_cos - i8_cos).abs() < 0.02, "f32={f32_cos} i8={i8_cos}");
    }

    #[test]
    fn quantized_cosine_is_scale_invariant() {
        // 同一方向不同模长 → 量化后余弦一致（尺度不变性）
        let v = vec![0.3f32, -0.6, 0.9, 0.1];
        let scaled: Vec<f32> = v.iter().map(|x| x * 7.5).collect();
        let qv = quantize_i8(&v);
        let qs = quantize_i8(&scaled);
        assert_eq!(qv.data, qs.data, "同方向量化分量应相同");
        // 与自身余弦 ≈ 1
        assert!((cosine_similarity_i8(&qv.data, &qs.data) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn zero_vector_quantization_is_safe() {
        let q = quantize_i8(&[0.0f32; 4]);
        assert_eq!(q.scale, 1.0);
        assert!(q.data.iter().all(|&d| d == 0));
        assert_eq!(cosine_similarity_i8(&q.data, &q.data), 0.0);
    }

    #[test]
    fn subnormal_scale_does_not_underflow_to_zero() {
        // review 修复回归：subnormal 级 max_abs（< ~1.78e-43）除以 127 会
        // 下溢为 0 → v/0 = ±Inf → round/clamp 饱和为 ±127（虚假大幅值）且
        // scale 字段为 0（调用方除零风险）。下溢时回退 scale=1.0。
        let tiny = f32::from_bits(1); // 最小正 subnormal（~1.4e-45）
        let q = quantize_i8(&[tiny, -tiny, 0.0]);
        assert_eq!(q.scale, 1.0, "scale must not underflow to zero");
        // subnormal 值无法在 i8 网格中表示（round 到 0），但不得产生
        // 虚假的饱和 ±127；反量化结果必须有限。
        assert!(q.data.iter().all(|&d| d == 0), "{:?}", q.data);
        let rebuilt = dequantize_i8(&q);
        assert!(rebuilt.iter().all(|x| x.is_finite()));
        // 正常量级不受影响。
        let normal = quantize_i8(&[1.0, -2.0, 0.5]);
        assert!(normal.scale > 0.0 && normal.scale.is_finite());
    }

    #[test]
    fn quantized_score_dispatches_by_metric() {
        let a = quantize_i8(&[1.0f32, 0.0, 0.0]);
        let b = quantize_i8(&[1.0f32, 0.0, 0.0]);
        assert!((quantized_score(Metric::Cosine, &a, &b) - 1.0).abs() < 1e-4);
        // Euclidean 同向同尺度 → 距离≈ 0 → 分数≈ 0
        assert!(quantized_score(Metric::Euclidean, &a, &b).abs() < 1e-3);
    }

    #[test]
    fn quantize_nan_is_safe_no_ub() {
        // NaN/Inf 输入不能触发 `NaN as i8` UB；分量映射为 0i8。
        let q = quantize_i8(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.0]);
        assert!(q.scale.is_finite());
        assert_eq!(q.data.len(), 4);
        assert_eq!(q.data[0], 0i8);
        assert_eq!(q.data[1], 0i8);
        assert_eq!(q.data[2], 0i8);
        // 正常值仍被正确量化
        assert!(q.data[3] != 0i8);
    }

    #[test]
    fn vector_from_value_rejects_numbers_outside_finite_f32_range() {
        assert!(vector_from_value(&serde_json::json!([1.25, -2.5])).is_ok());
        let error = vector_from_value(&serde_json::json!([1e100])).unwrap_err();
        assert!(matches!(error, MemoryError::Serialization(_)));
    }

    #[test]
    fn cosine_i8_self_similarity_is_exactly_one() {
        // 自相似度经 f64 除法 + clamp 应精确为 1.0
        let v = quantize_i8(&[0.3f32, -0.6, 0.9, 0.1, 0.5, -0.2, 0.8, -0.4]);
        let cos = cosine_similarity_i8(&v.data, &v.data);
        assert!((cos - 1.0).abs() < 1e-6, "self-cosine={cos}");
        // 任意向量与自身余弦 ≤ 1.0（clamp 保证）
        assert!(cos <= 1.0);
    }
}
