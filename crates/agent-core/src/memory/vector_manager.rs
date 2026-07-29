//! 多 Namespace 向量索引聚合的默认实现（AINS_PLAN 4.2 VectorIndexManager）。
//!
//! 每个 namespace 一个独立索引实例；平台差异（HNSW vs 线性）只出现在
//! 构造点的 cfg 分支，其余逻辑双端共享。

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::MemoryError;
use crate::memory::kv::KvStore;
use crate::memory::vector::{MemoryNamespace, VectorIndex, VectorIndexConfig, VectorIndexManager};

/// 默认 VectorIndexManager：持有 `embeddings`（Source Of Truth）与
/// `hnsw_cache`（Native 派生缓存；Web 端传同名空表即可）。
pub struct DefaultVectorIndexManager {
    embeddings: Arc<dyn KvStore>,
    hnsw_cache: Arc<dyn KvStore>,
    indexes: HashMap<MemoryNamespace, Box<dyn VectorIndex>>,
}

impl DefaultVectorIndexManager {
    pub fn new(embeddings: Arc<dyn KvStore>, hnsw_cache: Arc<dyn KvStore>) -> Self {
        Self {
            embeddings,
            hnsw_cache,
            indexes: HashMap::new(),
        }
    }

    /// 构造平台索引并从 `embeddings` 加载既有向量（启动加载语义）。
    async fn build_index(
        &self,
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
    ) -> Result<Box<dyn VectorIndex>, MemoryError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let index = crate::memory::vector_native::HnswVectorIndex::load(
                namespace,
                config,
                &*self.embeddings,
                &*self.hnsw_cache,
            )
            .await?;
            Ok(Box::new(index))
        }
        #[cfg(target_arch = "wasm32")]
        {
            let index = crate::memory::vector_web::LinearVectorIndex::load(
                namespace,
                config,
                &*self.embeddings,
            )
            .await?;
            Ok(Box::new(index))
        }
    }

    /// 将指定 namespace 的派生数据落盘（Native 写 hnsw_cache；Web no-op）。
    pub async fn save_index(&self, namespace: MemoryNamespace) -> Result<(), MemoryError> {
        let index = self
            .indexes
            .get(&namespace)
            .ok_or(MemoryError::NamespaceNotFound(namespace))?;
        index.save(&*self.hnsw_cache).await
    }

    /// 关闭前保存全部索引的派生数据。
    pub async fn save_all(&self) -> Result<(), MemoryError> {
        for index in self.indexes.values() {
            index.save(&*self.hnsw_cache).await?;
        }
        Ok(())
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl VectorIndexManager for DefaultVectorIndexManager {
    async fn get_index(&self, namespace: MemoryNamespace) -> Result<&dyn VectorIndex, MemoryError> {
        self.indexes
            .get(&namespace)
            .map(|b| &**b)
            .ok_or(MemoryError::NamespaceNotFound(namespace))
    }

    async fn get_index_mut(
        &mut self,
        namespace: MemoryNamespace,
    ) -> Result<&mut dyn VectorIndex, MemoryError> {
        match self.indexes.get_mut(&namespace) {
            Some(index) => Ok(index.as_mut()),
            None => Err(MemoryError::NamespaceNotFound(namespace)),
        }
    }

    async fn create_index(
        &mut self,
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
    ) -> Result<(), MemoryError> {
        let index = self.build_index(namespace, config).await?;
        self.indexes.insert(namespace, index);
        Ok(())
    }

    async fn remove_index(&mut self, namespace: MemoryNamespace) -> Result<(), MemoryError> {
        // 幂等（ensure-absent）：索引实例可能已在上一次失败的清理中移除，
        // 但存储行仍在，故无论实例是否存在都继续删除数据
        self.indexes.remove(&namespace);
        // 删除该 namespace 的全部向量（Source Of Truth，批量前缀删除，
        // 后端单事务）与派生缓存
        self.embeddings
            .delete_prefix(&namespace.storage_prefix())
            .await?;
        self.hnsw_cache
            .delete(&format!("hnsw/{}", namespace.as_str()))
            .await?;
        Ok(())
    }
}
