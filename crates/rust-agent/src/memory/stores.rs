//! MemoryStores：5 张逻辑表的统一句柄集合（AINS 向量表生产路径调用方设计 §6）。
//!
//! [`open_memory_stores`] 从已打开的 backend 构造全部表句柄，并完成统一的
//! 加密装配：
//! - `kv` 表沿用 legacy 兼容模式（AAD = storage_key），保证既有密文可读；
//! - `memories` / `embeddings` / `documents` / `hnsw_cache` 使用 table domain
//!   （AAD = `{table_name}\0{storage_key}`），共享相同 storage_key 的多表形成
//!   独立认证域，同 key 密文无法跨表搬运认证通过；
//! - 无密钥时不加装饰器（明文），保持既有行为。
//!
//! `MemoryEngine`、`MemoryService`、`DefaultVectorIndexManager` 必须使用同一组
//! 逻辑 store handles，不能一处走加密 wrapper、一处绕过 wrapper。

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock, Weak};

use crate::memory::engine::MemoryEngine;
use crate::memory::kv::{
    ALL_TABLES, KvStore, TABLE_DOCUMENTS, TABLE_EMBEDDINGS, TABLE_HNSW_CACHE, TABLE_KV,
    TABLE_MEMORIES,
};
use crate::memory::kv_crypto::{EncryptedKvStore, EncryptionKey};
use crate::memory::vector_manager::DefaultVectorIndexManager;

/// 同一持久化会话在一个运行时内共享的 extraction 协调状态。
///
/// gate 负责串行化执行；epoch 在会话清空时递增，使已经排队但尚未取得
/// gate 的旧任务失效。两者必须同属 `MemoryStores`，不能由每个
/// `MemoryService` 自行持有，否则恢复同一 session 的另一个实例仍会写回。
pub(crate) struct ExtractionSessionState {
    pub(crate) gate: futures::lock::Mutex<()>,
    pub(crate) epoch: AtomicU64,
}

/// 5 张逻辑表的统一句柄集合（1 张通用 KV 表 + 4 张 Memory/Vector 相关表）。
#[derive(Clone)]
pub struct MemoryStores {
    /// 通用状态、检查点、MemoryService 状态。
    pub kv: Arc<dyn KvStore>,
    /// `MemoryEntry` 内容与 metadata（长期记忆 SoT 的内容侧）。
    pub memories: Arc<dyn KvStore>,
    /// f32 embedding（向量 SoT）。
    pub embeddings: Arc<dyn KvStore>,
    /// 文档 metadata / hash 映射。
    pub documents: Arc<dyn KvStore>,
    /// Native HNSW 派生缓存；Web 为兼容空表。
    pub hnsw_cache: Arc<dyn KvStore>,
    /// 所有使用同一 `MemoryStores` 的 session 共享的 engine / vector index。
    /// 这既让已物化的索引收到其他 session 的写入，也让 scoped signature
    /// 检查与写入处在同一 async mutex 临界区，避免跨 session 的重复创建。
    pub engine: Arc<futures::lock::Mutex<MemoryEngine>>,
    /// 所有会话共享的可召回内容版本。任一 durable memory / project document
    /// 写入都会递增，供各自的 prompt cache 失效，避免跨 session 写入后仍在
    /// 缓存窗口内返回旧召回结果。
    pub revision: Arc<AtomicU64>,
    /// embedding contract 的线性化门闩。`MemoryService` 是每 session 一个
    /// 实例，而 contract / vector index 属于共享 stores：首次 embed 的
    /// "读 contract → 创建/校验 → 登记索引" 必须跨 session 原子完成，不能
    /// 让两个 profile 或维度在同一个空 KV 槽位上竞态写入。
    pub embedding_contract_gate: Arc<futures::lock::Mutex<()>>,
    /// 项目文档索引的跨 session 门闩。`LocalDocumentStore` 的 source-hash
    /// 去重由“读取 hash 映射 → 写 chunks/meta/hash”组成，必须由共享门闩
    /// 串行化；否则两个 `MemoryService` 可同时看到缺失映射并留下重复 chunks。
    pub document_index_gate: Arc<futures::lock::Mutex<()>>,
    /// 同一持久化 session 的 durable extraction 门闩。服务实例不是可靠的
    /// session 边界：同一进程可以同时恢复同一个 snapshot，因此 gate 必须由
    /// 共享 stores 持有。Weak 值会在最后一个 session service 释放后自然回收，
    /// 避免历史 session id 使 map 无界增长。
    extraction_sessions: Arc<RwLock<HashMap<String, Weak<ExtractionSessionState>>>>,
    /// 跨会话 durable 写入/删除门闩。管理页的“清空全部”影响的是当前
    /// owner/project 的所有会话，不能只与某一个 session 的 extraction gate
    /// 串行，否则正在执行的另一会话抽取会在清空后重新写入数据。
    pub(crate) durable_mutation_gate: Arc<futures::lock::Mutex<()>>,
}

impl MemoryStores {
    /// 从已装配的五张表创建共享运行时。测试与宿主装配都应使用该构造器，
    /// 不要手工拼 `MemoryStores` 而意外得到每 session 独立的 engine。
    #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
    pub fn from_parts(
        kv: Arc<dyn KvStore>,
        memories: Arc<dyn KvStore>,
        embeddings: Arc<dyn KvStore>,
        documents: Arc<dyn KvStore>,
        hnsw_cache: Arc<dyn KvStore>,
    ) -> Self {
        let manager =
            DefaultVectorIndexManager::new(Arc::clone(&embeddings), Arc::clone(&hnsw_cache));
        let engine = MemoryEngine::new(
            Arc::clone(&memories),
            Arc::clone(&embeddings),
            Box::new(manager),
        );
        Self {
            kv,
            memories,
            embeddings,
            documents,
            hnsw_cache,
            engine: Arc::new(futures::lock::Mutex::new(engine)),
            revision: Arc::new(AtomicU64::new(0)),
            embedding_contract_gate: Arc::new(futures::lock::Mutex::new(())),
            document_index_gate: Arc::new(futures::lock::Mutex::new(())),
            extraction_sessions: Arc::new(RwLock::new(HashMap::new())),
            durable_mutation_gate: Arc::new(futures::lock::Mutex::new(())),
        }
    }

    /// Return the process-shared durable-extraction gate for one persisted
    /// session identity. Web hosts add an origin-wide lock around this gate to
    /// cover separate tabs; this map covers independent service instances in
    /// the same runtime (including Native hosts).
    pub(crate) fn extraction_session_for(&self, session_key: &str) -> Arc<ExtractionSessionState> {
        let mut sessions = self
            .extraction_sessions
            .write()
            .unwrap_or_else(|poison| poison.into_inner());
        sessions.retain(|_, session| session.strong_count() > 0);
        if let Some(session) = sessions.get(session_key).and_then(Weak::upgrade) {
            return session;
        }
        let session = Arc::new(ExtractionSessionState {
            gate: futures::lock::Mutex::new(()),
            epoch: AtomicU64::new(0),
        });
        sessions.insert(session_key.to_string(), Arc::downgrade(&session));
        session
    }
}

/// 平台存储后端句柄（§6.1）：app 层首次打开后缓存 backend handle；
/// native 不得为 MemoryService 二次打开同一 redb 文件（单进程独占锁），
/// web 保持同一抽象。
pub enum MemoryBackend {
    #[cfg(not(target_arch = "wasm32"))]
    Native(Arc<crate::memory::kv_native::RedbBackend>),
    #[cfg(target_arch = "wasm32")]
    Web(Arc<crate::memory::kv_web::IndexedDbBackend>),
}

impl MemoryBackend {
    /// 绑定指定逻辑表的 `KvStore` 句柄（双端统一入口）。
    fn table(&self, name: &str) -> Arc<dyn KvStore> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native(backend) => Arc::new(backend.table(name)),
            #[cfg(target_arch = "wasm32")]
            Self::Web(backend) => Arc::new(backend.store(name)),
        }
    }
}

/// 从已打开的 backend 构造 5 张逻辑表的统一句柄集合（§6.2）。
///
/// `key` 为可选加密密钥：`kv` 表沿用 legacy 兼容模式（AAD = storage_key，
/// 既有密文可读），其余 4 张 Memory 表使用 table domain 模式
/// （AAD = `{table_name}\0{storage_key}`）。无密钥时不加装饰器（明文）。
pub fn open_memory_stores(backend: &MemoryBackend, key: Option<EncryptionKey>) -> MemoryStores {
    build_stores(|name| backend.table(name), key)
}

/// 在为已有数据库启用静态加密前进行显式转换保护。
///
/// 空表或能用当前 key + table domain 解密的密文表可直接启用。任一明文、
/// 错误密钥、旧 AAD 域或篡改密文都会默认 fail closed，避免 wrapper 在运行
/// 中才逐条失败。调用方只有在用户明确选择 reset 时才能清空全部 5 张逻辑表
/// 后继续启用加密；该 reset 是有意的不可逆操作，不能静默执行。
pub async fn prepare_encryption(
    backend: &MemoryBackend,
    key: &EncryptionKey,
    reset_plaintext: bool,
) -> Result<(), crate::error::MemoryError> {
    let mut plaintext_tables = Vec::new();
    let mut unreadable_tables = Vec::new();
    for table_name in ALL_TABLES {
        let table = backend.table(table_name);
        let mut has_plaintext = false;
        let mut has_unreadable = false;
        for storage_key in table.list_prefix("").await? {
            if let Some(value) = table.get(&storage_key).await? {
                if !EncryptedKvStore::is_sealed_envelope(&value) {
                    has_plaintext = true;
                    continue;
                }
                let unsealed = if table_name == TABLE_KV {
                    key.unseal(&storage_key, &value)
                } else {
                    key.unseal_in_domain(table_name, &storage_key, &value)
                };
                if unsealed.is_err() {
                    has_unreadable = true;
                }
            }
        }
        if has_plaintext {
            plaintext_tables.push(table_name);
        }
        if has_unreadable {
            unreadable_tables.push(table_name);
        }
    }
    if plaintext_tables.is_empty() && unreadable_tables.is_empty() {
        return Ok(());
    }
    if !reset_plaintext {
        return Err(crate::error::MemoryError::Encryption(format!(
            "refusing encryption transition: plaintext tables ({}) unreadable encrypted tables ({}) require an explicit migration or reset",
            if plaintext_tables.is_empty() {
                "none".to_string()
            } else {
                plaintext_tables.join(", ")
            },
            if unreadable_tables.is_empty() {
                "none".to_string()
            } else {
                unreadable_tables.join(", ")
            },
        )));
    }
    for table_name in ALL_TABLES {
        backend.table(table_name).delete_prefix("").await?;
    }
    tracing::warn!(
        plaintext_tables = ?plaintext_tables,
        unreadable_tables = ?unreadable_tables,
        "explicit storage-encryption reset removed existing incompatible data"
    );
    Ok(())
}

/// 统一装配：`kv` 用 legacy 兼容模式，其余 4 表用 table domain 模式。
/// wasm 单线程下 Arc 包装的 `dyn KvStore` 非 Send/Sync 无害。
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
fn build_stores(
    raw: impl Fn(&str) -> Arc<dyn KvStore>,
    key: Option<EncryptionKey>,
) -> MemoryStores {
    let wrap = |table_name: &str| -> Arc<dyn KvStore> {
        let store = raw(table_name);
        match &key {
            Some(key) => {
                let cloned = EncryptionKey::from_bytes(key.clone_bytes());
                if table_name == TABLE_KV {
                    Arc::new(EncryptedKvStore::new(store, cloned))
                } else {
                    Arc::new(EncryptedKvStore::with_table_domain(
                        store, cloned, table_name,
                    ))
                }
            }
            None => store,
        }
    };
    MemoryStores::from_parts(
        wrap(TABLE_KV),
        wrap(TABLE_MEMORIES),
        wrap(TABLE_EMBEDDINGS),
        wrap(TABLE_DOCUMENTS),
        wrap(TABLE_HNSW_CACHE),
    )
}
