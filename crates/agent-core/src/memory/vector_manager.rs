//! 多 Namespace 向量索引聚合的默认实现（AINS_PLAN 4.2 VectorIndexManager
//! + 7.4 冷启动懒加载）。
//!
//! 每个 namespace 一个独立索引实例，以 [`IndexSlot`] 表达「已登记未物化」
//! (`Pending`) 与「已重建入内存」(`Loaded`) 两态：
//! - `create_index` 只把 slot 置为 `Pending`（记录配置），**零 I/O、零图重建**；
//! - 首次 `search` 命中该 namespace 时才从 `embeddings`（Source Of Truth）
//!   重建索引并转为 `Loaded`（懒加载）；
//! - `add`/`remove` 对 `Pending` slot 为 no-op——引擎遵循「SoT 先行」写入
//!   （先落盘 `embeddings` 再调用索引），故惰性重建时会自然纳入 / 排除。
//!
//! 冷启动收益：进程启动为全部 namespace `create_index` 不再触发向量整表
//! 加载与图重建；只有会话中实际被检索的 namespace 才付出一次重建成本。
//!
//! `search` 走共享引用 `&self`，其惰性物化需要内部可变性；`Pending → Loaded`
//! 的转换涉及 `.await`（从 KvStore 读取），因此每个 slot 以 `futures::lock::Mutex`
//! 包裹（可跨 `.await` 持有，双 target 通用，不触发 `await_holding_lock`）。
//! 平台差异（HNSW vs 线性）只出现在构造点的 cfg 分支，其余逻辑双端共享。

use std::collections::HashMap;
use std::sync::Arc;

use futures::lock::Mutex;

use crate::error::MemoryError;
use crate::memory::kv::KvStore;
use crate::memory::vector::{MemoryNamespace, VectorIndex, VectorIndexConfig, VectorIndexManager};

/// 单 namespace 索引槽位的两态（懒加载）。
enum IndexSlot {
    /// 已登记未物化：仅持有配置，尚未从 `embeddings` 重建。
    Pending(VectorIndexConfig),
    /// 已物化：内存中的平台索引实例。
    Loaded {
        config: VectorIndexConfig,
        index: Box<dyn VectorIndex>,
    },
}

/// 默认 VectorIndexManager：持有 `embeddings`（Source Of Truth）与
/// `hnsw_cache`（Native 派生缓存；Web 端传同名空表即可）。索引按 namespace
/// 懒加载（见模块文档）。
pub struct DefaultVectorIndexManager {
    embeddings: Arc<dyn KvStore>,
    hnsw_cache: Arc<dyn KvStore>,
    indexes: HashMap<MemoryNamespace, Mutex<IndexSlot>>,
}

impl DefaultVectorIndexManager {
    pub fn new(embeddings: Arc<dyn KvStore>, hnsw_cache: Arc<dyn KvStore>) -> Self {
        Self {
            embeddings,
            hnsw_cache,
            indexes: HashMap::new(),
        }
    }

    /// 从 `embeddings`（Source Of Truth）重建平台索引（懒加载物化路径）。
    async fn build_index(
        embeddings: &dyn KvStore,
        hnsw_cache: &dyn KvStore,
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
    ) -> Result<Box<dyn VectorIndex>, MemoryError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let index = crate::memory::vector_native::HnswVectorIndex::load(
                namespace, config, embeddings, hnsw_cache,
            )
            .await?;
            Ok(Box::new(index))
        }
        #[cfg(target_arch = "wasm32")]
        {
            // Web linear index has no derived HNSW cache, but keep this
            // shared helper signature identical across targets.
            let _ = hnsw_cache;
            let index =
                crate::memory::vector_web::LinearVectorIndex::load(namespace, config, embeddings)
                    .await?;
            Ok(Box::new(index))
        }
    }

    /// 测试 / 观测辅助：该 namespace 索引是否已物化（`Loaded`）。
    /// 未登记或仍为 `Pending` 均返回 false。
    pub fn is_loaded(&self, namespace: MemoryNamespace) -> bool {
        self.indexes
            .get(&namespace)
            .and_then(|slot| slot.try_lock())
            .is_some_and(|guard| matches!(&*guard, IndexSlot::Loaded { .. }))
    }

    /// 将指定 namespace 的派生数据落盘（Native 写 hnsw_cache；Web no-op）。
    /// 尚未物化（`Pending`）的索引无内存派生数据可持久化，跳过。
    pub async fn save_index(&self, namespace: MemoryNamespace) -> Result<(), MemoryError> {
        let slot = self
            .indexes
            .get(&namespace)
            .ok_or(MemoryError::NamespaceNotFound(namespace))?;
        match &*slot.lock().await {
            IndexSlot::Loaded { index, .. } => index.save(&*self.hnsw_cache).await,
            IndexSlot::Pending(_) => Ok(()),
        }
    }

    /// 关闭前保存全部**已物化**索引的派生数据（`Pending` 跳过）。
    pub async fn save_all(&self) -> Result<(), MemoryError> {
        for slot in self.indexes.values() {
            if let IndexSlot::Loaded { index, .. } = &*slot.lock().await {
                index.save(&*self.hnsw_cache).await?;
            }
        }
        Ok(())
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl VectorIndexManager for DefaultVectorIndexManager {
    async fn create_index(
        &mut self,
        namespace: MemoryNamespace,
        config: VectorIndexConfig,
    ) -> Result<(), MemoryError> {
        // 懒加载：仅登记配置，不做任何 I/O、不重建图（冷启动优化核心）。
        self.indexes
            .insert(namespace, Mutex::new(IndexSlot::Pending(config)));
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

    async fn add(
        &mut self,
        namespace: MemoryNamespace,
        node_id: &str,
        vector: &[f32],
    ) -> Result<(), MemoryError> {
        // 独占 `&mut self`，用 get_mut() 直取 slot 内部（无需异步加锁）。
        // Clone stores before borrowing the slot so a backend-specific rebuild
        // can safely materialize a replacement from SoT after the borrow ends.
        let embeddings = Arc::clone(&self.embeddings);
        let hnsw_cache = Arc::clone(&self.hnsw_cache);
        match self.indexes.get_mut(&namespace) {
            Some(slot) => {
                let (result, rebuild_required, config) = match slot.get_mut() {
                    IndexSlot::Loaded { config, index } => {
                        let result = index.add(node_id, vector).await;
                        let rebuild_required = result.is_err() && index.take_rebuild_required();
                        (result, rebuild_required, Some(config.clone()))
                    }
                    // 未物化：不插入（惰性重建时从 SoT 自然纳入），但仍校验维度
                    // ——与已物化路径的写入校验同口径，错误向量不得“写时静默、
                    // 物化时才被跳过”（使上层能及时回滚 SoT）。
                    IndexSlot::Pending(config) => {
                        if vector.len() != config.dimension as usize {
                            return Err(MemoryError::Storage(format!(
                                "dimension mismatch: expected {}, got {}",
                                config.dimension,
                                vector.len()
                            )));
                        }
                        return Ok(());
                    }
                };
                if !rebuild_required {
                    return result;
                }
                // The engine persists embeddings before calling add.  Native
                // HNSW physical-slot exhaustion is therefore recoverable by
                // rebuilding the derived graph, which includes this write.
                let config = config.expect("loaded index has a config");
                let rebuilt =
                    Self::build_index(&*embeddings, &*hnsw_cache, namespace, config.clone())
                        .await?;
                *slot.get_mut() = IndexSlot::Loaded {
                    config,
                    index: rebuilt,
                };
                Ok(())
            }
            None => Err(MemoryError::NamespaceNotFound(namespace)),
        }
    }

    async fn remove(
        &mut self,
        namespace: MemoryNamespace,
        node_id: &str,
    ) -> Result<(), MemoryError> {
        match self.indexes.get_mut(&namespace) {
            // 调用方已从 SoT 删除；未物化索引惰性重建时自然不再纳入，本调用 no-op。
            Some(slot) => match slot.get_mut() {
                IndexSlot::Loaded { index, .. } => index.remove(node_id).await,
                IndexSlot::Pending(_) => Ok(()),
            },
            None => Err(MemoryError::NamespaceNotFound(namespace)),
        }
    }

    async fn search(
        &self,
        namespace: MemoryNamespace,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<(String, f32)>, MemoryError> {
        let slot = self
            .indexes
            .get(&namespace)
            .ok_or(MemoryError::NamespaceNotFound(namespace))?;
        // 跨 .await 持有 futures 异步锁：物化（读 KvStore + 重建图）与检索
        // 均在锁内完成，保证并发首查只重建一次。
        let mut guard = slot.lock().await;
        if let IndexSlot::Pending(config) = &*guard {
            // 物化前校验查询参数（review 修复）：错误查询（top_k=0 / 维度不符）
            // 直接短路，不触发从 SoT 的整表重建——历史实现先重建再检索，
            // 错误查询会白白付出一次全量重建成本，可被外部输入反复触发。
            if top_k == 0 {
                return Ok(Vec::new());
            }
            if query.len() != config.dimension as usize {
                return Err(MemoryError::Storage(format!(
                    "dimension mismatch: expected {}, got {}",
                    config.dimension,
                    query.len()
                )));
            }
            // 首次命中：从 embeddings（SoT）懒加载重建索引。
            let config = config.clone();
            let index = Self::build_index(
                &*self.embeddings,
                &*self.hnsw_cache,
                namespace,
                config.clone(),
            )
            .await?;
            *guard = IndexSlot::Loaded { config, index };
        }
        match &*guard {
            IndexSlot::Loaded { index, .. } => index.search(query, top_k).await,
            // 上一分支已把 Pending 物化为 Loaded，此处不可达。
            IndexSlot::Pending(_) => unreachable!("index materialized above"),
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use serde_json::Value;

    use crate::memory::vector::{Metric, vector_to_value};

    struct MockKv {
        data: StdMutex<HashMap<String, Value>>,
        /// list_prefix 调用计数（观测懒加载物化次数）。
        list_calls: std::sync::atomic::AtomicUsize,
        /// 注入：下一次 list_prefix 调用失败（模拟物化中途 I/O 失败）。
        fail_next_list: std::sync::atomic::AtomicBool,
    }

    impl MockKv {
        fn new() -> Self {
            Self {
                data: StdMutex::new(HashMap::new()),
                list_calls: std::sync::atomic::AtomicUsize::new(0),
                fail_next_list: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl KvStore for MockKv {
        async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
            Ok(self.data.lock().expect("mock lock").get(key).cloned())
        }

        async fn set(
            &self,
            key: &str,
            value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            self.data
                .lock()
                .expect("mock lock")
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), MemoryError> {
            self.data.lock().expect("mock lock").remove(key);
            Ok(())
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            if self
                .fail_next_list
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(MemoryError::Storage("injected list failure".into()));
            }
            self.list_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut keys: Vec<_> = self
                .data
                .lock()
                .expect("mock lock")
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect();
            keys.sort();
            Ok(keys)
        }
    }

    /// 模拟已报告“物理槽位耗尽”的 HNSW；用于验证管理器只在明确请求时
    /// 从 SoT 替换派生索引。
    struct RebuildRequestIndex;

    #[async_trait::async_trait]
    impl VectorIndex for RebuildRequestIndex {
        async fn add(&mut self, _node_id: &str, _vector: &[f32]) -> Result<(), MemoryError> {
            Err(MemoryError::Storage("physical slots exhausted".into()))
        }

        fn take_rebuild_required(&mut self) -> bool {
            true
        }

        async fn search(
            &self,
            _query: &[f32],
            _top_k: usize,
        ) -> Result<Vec<(String, f32)>, MemoryError> {
            Ok(Vec::new())
        }

        async fn remove(&mut self, _node_id: &str) -> Result<(), MemoryError> {
            Ok(())
        }

        async fn save(&self, _kv: &dyn KvStore) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    fn config() -> VectorIndexConfig {
        VectorIndexConfig {
            dimension: 2,
            distance_metric: Metric::Cosine,
            m: 16,
            ef: 50,
        }
    }

    #[tokio::test]
    async fn rebuild_signal_replaces_loaded_index_from_persisted_embeddings() {
        let embeddings: Arc<dyn KvStore> = Arc::new(MockKv::new());
        let cache: Arc<dyn KvStore> = Arc::new(MockKv::new());
        let namespace = MemoryNamespace::Personal;
        // SoT-first contract: the engine has already written the new vector
        // when index add reports its physical-slot exhaustion.
        embeddings
            .set(
                &namespace.storage_key("recovered"),
                &vector_to_value(&[1.0, 0.0]),
                None,
            )
            .await
            .unwrap();
        let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), cache);
        manager.indexes.insert(
            namespace,
            Mutex::new(IndexSlot::Loaded {
                config: config(),
                index: Box::new(RebuildRequestIndex),
            }),
        );

        manager
            .add(namespace, "recovered", &[1.0, 0.0])
            .await
            .unwrap();

        let hits = manager.search(namespace, &[1.0, 0.0], 1).await.unwrap();
        assert_eq!(hits.first().map(|hit| hit.0.as_str()), Some("recovered"));
    }

    fn pending_manager() -> (DefaultVectorIndexManager, Arc<MockKv>) {
        let embeddings = Arc::new(MockKv::new());
        let cache: Arc<dyn KvStore> = Arc::new(MockKv::new());
        let mut manager =
            DefaultVectorIndexManager::new(Arc::clone(&embeddings) as Arc<dyn KvStore>, cache);
        futures::executor::block_on(manager.create_index(MemoryNamespace::Personal, config()))
            .unwrap();
        (manager, embeddings)
    }

    #[tokio::test]
    async fn pending_search_with_top_k_zero_does_not_materialize() {
        // 错误查询不得触发整表重建（review 修复）：top_k=0 直接返回空。
        let (manager, embeddings) = pending_manager();
        assert!(!manager.is_loaded(MemoryNamespace::Personal));
        let hits = manager
            .search(MemoryNamespace::Personal, &[1.0, 0.0], 0)
            .await
            .unwrap();
        assert!(hits.is_empty());
        assert!(
            !manager.is_loaded(MemoryNamespace::Personal),
            "top_k=0 不得触发物化"
        );
        assert_eq!(
            embeddings
                .list_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "top_k=0 不得读 SoT"
        );
    }

    #[tokio::test]
    async fn pending_search_with_wrong_dimension_does_not_materialize() {
        // 维度不符的查询在物化前即报错，不触发整表重建（review 修复）。
        let (manager, embeddings) = pending_manager();
        let err = manager
            .search(MemoryNamespace::Personal, &[1.0, 0.0, 0.0], 1)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, MemoryError::Storage(msg) if msg.contains("dimension mismatch")),
            "{err:?}"
        );
        assert!(
            !manager.is_loaded(MemoryNamespace::Personal),
            "维度错误不得触发物化"
        );
        assert_eq!(
            embeddings
                .list_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "维度错误不得读 SoT"
        );
    }

    #[tokio::test]
    async fn concurrent_first_search_materializes_exactly_once() {
        // 并发首查：per-slot 异步锁保证只重建一次（review 测试补充）。
        let (manager, embeddings) = pending_manager();
        embeddings
            .set(
                &MemoryNamespace::Personal.storage_key("a"),
                &vector_to_value(&[1.0, 0.0]),
                None,
            )
            .await
            .unwrap();
        embeddings
            .set(
                &MemoryNamespace::Personal.storage_key("b"),
                &vector_to_value(&[0.0, 1.0]),
                None,
            )
            .await
            .unwrap();
        let (r1, r2) = tokio::join!(
            manager.search(MemoryNamespace::Personal, &[1.0, 0.0], 1),
            manager.search(MemoryNamespace::Personal, &[1.0, 0.0], 1),
        );
        let hits1 = r1.unwrap();
        let hits2 = r2.unwrap();
        assert_eq!(hits1.first().map(|h| h.0.as_str()), Some("a"));
        assert_eq!(hits2.first().map(|h| h.0.as_str()), Some("a"));
        assert!(manager.is_loaded(MemoryNamespace::Personal));
        assert_eq!(
            embeddings
                .list_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "并发首查必须只重建一次"
        );
    }

    #[tokio::test]
    async fn load_failure_keeps_pending_and_retry_succeeds() {
        // 物化中途 I/O 失败：槽位保持 Pending，下次 search 重试成功
        // （review 测试补充：不残留部分物化状态）。
        let (manager, embeddings) = pending_manager();
        embeddings
            .set(
                &MemoryNamespace::Personal.storage_key("a"),
                &vector_to_value(&[1.0, 0.0]),
                None,
            )
            .await
            .unwrap();
        embeddings
            .fail_next_list
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = manager
            .search(MemoryNamespace::Personal, &[1.0, 0.0], 1)
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::Storage(_)), "{err:?}");
        assert!(
            !manager.is_loaded(MemoryNamespace::Personal),
            "失败后必须保持 Pending"
        );
        // 重试：不再注入失败，重建成功。
        let hits = manager
            .search(MemoryNamespace::Personal, &[1.0, 0.0], 1)
            .await
            .unwrap();
        assert_eq!(hits.first().map(|h| h.0.as_str()), Some("a"));
        assert!(manager.is_loaded(MemoryNamespace::Personal));
        assert_eq!(
            embeddings
                .list_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "失败的注入读取不计入；仅成功重试计 1 次"
        );
    }

    #[tokio::test]
    async fn remove_on_pending_is_noop_and_stays_pending() {
        // 未物化索引上的 remove 是 no-op（SoT 先行契约：惰性重建自然排除），
        // 且不得触发物化（review 测试补充）。
        let (mut manager, embeddings) = pending_manager();
        manager
            .remove(MemoryNamespace::Personal, "ghost")
            .await
            .unwrap();
        assert!(!manager.is_loaded(MemoryNamespace::Personal));
        assert_eq!(
            embeddings
                .list_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}
