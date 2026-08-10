//! Native 端 KvStore 实现：redb（纯 Rust 嵌入式 KV 引擎，本地文件）。
//!
//! 一个 [`RedbBackend`] 对应一个 redb 数据库文件，内部按 AINS_PLAN 4.1 的
//! 存储结构划分逻辑表；[`RedbKvStore`] 是绑定单个逻辑表的 `KvStore` 句柄，
//! 多个句柄共享同一 `Arc<redb::Database>`。
//!
//! TTL 语义：读时检查（`get` 惰性删除过期条目，`list_prefix` 仅跳过）+
//! 后台定期清理（`sweep_expired`，由 `ttl.rs` 的 sweeper 驱动）。
//!
//! 阻塞语义：redb 调用为同步阻塞（单写者锁，`begin_write` 会等待
//! 其他写事务），嵌入式记忆规模下在 async 上下文直接调用可接受；
//! 若未来复用到高并发服务端热路径，应包一层 `spawn_blocking`。

#![cfg(not(target_arch = "wasm32"))]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use redb::{Database, ReadableTable, TableDefinition, TableError};
use serde_json::Value;

use crate::error::MemoryError;
use crate::memory::kv::{ALL_TABLES, Envelope, KvStore, now_ms};

fn storage_err(e: impl std::fmt::Display) -> MemoryError {
    MemoryError::Storage(e.to_string())
}

fn table_def(name: &str) -> TableDefinition<'_, &'static str, &'static [u8]> {
    TableDefinition::new(name)
}

/// redb 数据库句柄（Native 存储后端入口）。
#[derive(Clone)]
pub struct RedbBackend {
    db: Arc<Database>,
}

impl RedbBackend {
    /// 打开（或创建）本地 redb 数据库，并确保全部逻辑表存在。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let db = Database::create(path).map_err(storage_err)?;
        let write = db.begin_write().map_err(storage_err)?;
        for name in ALL_TABLES {
            write.open_table(table_def(name)).map_err(storage_err)?;
        }
        write.commit().map_err(storage_err)?;
        Ok(Self { db: Arc::new(db) })
    }

    /// 获取绑定指定逻辑表的 `KvStore` 句柄。
    pub fn table(&self, name: &str) -> RedbKvStore {
        RedbKvStore {
            db: Arc::clone(&self.db),
            table: name.to_string(),
        }
    }
}

/// 绑定单个逻辑表的 redb `KvStore` 实现。
pub struct RedbKvStore {
    db: Arc<Database>,
    table: String,
}

impl RedbKvStore {
    /// 便捷构造：独立打开数据库并绑定 `kv` 表（对齐计划中的 `default_kv_store`）。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        Ok(RedbBackend::open(path)?.table(crate::memory::kv::TABLE_KV))
    }

    fn read_raw(&self, key: &str) -> Result<Option<Envelope>, MemoryError> {
        let read = self.db.begin_read().map_err(storage_err)?;
        let table = match read.open_table(table_def(&self.table)) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(storage_err(e)),
        };
        let Some(guard) = table.get(key).map_err(storage_err)? else {
            return Ok(None);
        };
        Ok(Some(Envelope::decode(guard.value())?))
    }

    fn remove_raw(&self, key: &str) -> Result<(), MemoryError> {
        let write = self.db.begin_write().map_err(storage_err)?;
        {
            let mut table = write
                .open_table(table_def(&self.table))
                .map_err(storage_err)?;
            table.remove(key).map_err(storage_err)?;
        }
        write.commit().map_err(storage_err)
    }

    /// 惰性过期删除：写事务内复核过期状态后再删。读事务判定过期与
    /// 写事务删除之间可能有并发 `set` 刷新同 key，无条件删除会丢新值。
    fn remove_if_expired(&self, key: &str, now: i64) -> Result<(), MemoryError> {
        let write = self.db.begin_write().map_err(storage_err)?;
        {
            let mut table = write
                .open_table(table_def(&self.table))
                .map_err(storage_err)?;
            let expired = match table.get(key).map_err(storage_err)? {
                Some(guard) => Envelope::decode(guard.value())
                    .map(|env| env.is_expired(now))
                    .unwrap_or(false),
                None => false,
            };
            if expired {
                table.remove(key).map_err(storage_err)?;
            }
        }
        write.commit().map_err(storage_err)
    }
}

#[async_trait::async_trait]
impl KvStore for RedbKvStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
        let now = now_ms();
        match self.read_raw(key)? {
            None => Ok(None),
            Some(env) if env.is_expired(now) => {
                // 读时检查：惰性删除过期条目（写事务内复核，避免误删
                // 并发刷新的新值）
                self.remove_if_expired(key, now)?;
                Ok(None)
            }
            Some(env) => Ok(Some(env.value()?)),
        }
    }

    async fn set(
        &self,
        key: &str,
        value: &Value,
        ttl: Option<Duration>,
    ) -> Result<(), MemoryError> {
        let bytes = Envelope::new(value, ttl, now_ms()).encode()?;
        let write = self.db.begin_write().map_err(storage_err)?;
        {
            let mut table = write
                .open_table(table_def(&self.table))
                .map_err(storage_err)?;
            table.insert(key, bytes.as_slice()).map_err(storage_err)?;
        }
        write.commit().map_err(storage_err)
    }

    async fn delete(&self, key: &str) -> Result<(), MemoryError> {
        self.remove_raw(key)
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
        let now = now_ms();
        let read = self.db.begin_read().map_err(storage_err)?;
        let table = match read.open_table(table_def(&self.table)) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(storage_err(e)),
        };
        let mut keys = Vec::new();
        for item in table.range(prefix..).map_err(storage_err)? {
            let (key, value) = item.map_err(storage_err)?;
            let key = key.value();
            if !key.starts_with(prefix) {
                break;
            }
            // 单行损坏不应毒化整表扫描：跳过无法解码的行（M4）
            match Envelope::decode(value.value()) {
                Ok(env) if env.is_expired(now) => continue,
                Ok(_) => {}
                Err(_) => continue,
            }
            keys.push(key.to_string());
        }
        Ok(keys)
    }

    async fn sweep_expired(&self) -> Result<u64, MemoryError> {
        let now = now_ms();
        let mut expired = Vec::new();
        {
            let read = self.db.begin_read().map_err(storage_err)?;
            let table = match read.open_table(table_def(&self.table)) {
                Ok(t) => t,
                Err(TableError::TableDoesNotExist(_)) => return Ok(0),
                Err(e) => return Err(storage_err(e)),
            };
            for item in table.iter().map_err(storage_err)? {
                let (key, value) = item.map_err(storage_err)?;
                // 损坏行跳过、不自动删除（格式演进时避免误删，M4）
                if let Ok(env) = Envelope::decode(value.value())
                    && env.is_expired(now)
                {
                    expired.push(key.value().to_string());
                }
            }
        }
        if expired.is_empty() {
            return Ok(0);
        }
        let mut removed = 0u64;
        let write = self.db.begin_write().map_err(storage_err)?;
        {
            let mut table = write
                .open_table(table_def(&self.table))
                .map_err(storage_err)?;
            for key in &expired {
                // 写事务内复核：扫描与删除之间可能有并发 set 刷新同 key
                let still_expired = match table.get(key.as_str()).map_err(storage_err)? {
                    Some(guard) => Envelope::decode(guard.value())
                        .map(|env| env.is_expired(now))
                        .unwrap_or(false),
                    None => false,
                };
                if still_expired {
                    table.remove(key.as_str()).map_err(storage_err)?;
                    removed += 1;
                }
            }
        }
        write.commit().map_err(storage_err)?;
        Ok(removed)
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<u64, MemoryError> {
        // 单写事务批量删除（默认实现为逐键 N 个写事务）；不解码
        // Envelope，过期与损坏行一并清除（clear 语义彻底）。
        let write = self.db.begin_write().map_err(storage_err)?;
        let mut removed = 0u64;
        {
            let mut table = write
                .open_table(table_def(&self.table))
                .map_err(storage_err)?;
            let mut keys = Vec::new();
            for item in table.range(prefix..).map_err(storage_err)? {
                let (key, _) = item.map_err(storage_err)?;
                let key = key.value();
                if !key.starts_with(prefix) {
                    break;
                }
                keys.push(key.to_string());
            }
            for key in &keys {
                table.remove(key.as_str()).map_err(storage_err)?;
                removed += 1;
            }
        }
        write.commit().map_err(storage_err)?;
        Ok(removed)
    }
}
