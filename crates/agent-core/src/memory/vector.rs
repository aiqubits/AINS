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

    /// 相似度检索，返回 `(node_id, score)`，score 方向与 Metric 语义一致。
    async fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<(String, f32)>, MemoryError>;

    async fn remove(&mut self, node_id: &str) -> Result<(), MemoryError>;

    /// 持久化派生数据（Native：HNSW 图拓扑写入 `hnsw_cache`；Web：no-op，
    /// write-through 已保证 `embeddings` 落盘）。
    async fn save(&self, kv: &dyn KvStore) -> Result<(), MemoryError>;
}

/// 多 Namespace 向量索引聚合；上层必须先 `get_index(namespace)` 再检索，
/// 不存在跨 namespace 的单一入口。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait VectorIndexManager: MaybeSendSync {
    /// 获取指定 namespace 的向量索引（不存在则返回 Err）。
    async fn get_index(&self, namespace: MemoryNamespace) -> Result<&dyn VectorIndex, MemoryError>;

    /// 获取指定 namespace 向量索引的可变引用（写入路径）。
    async fn get_index_mut(
        &mut self,
        namespace: MemoryNamespace,
    ) -> Result<&mut dyn VectorIndex, MemoryError>;

    /// 为指定 namespace 创建新的向量索引。
    async fn create_index(
        &mut self,
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
    ) -> Result<(), MemoryError>;

    /// 删除指定 namespace 的向量索引（含所有节点）。
    async fn remove_index(&mut self, namespace: MemoryNamespace) -> Result<(), MemoryError>;
}
