//! 进程内内存 KV 存储（双端通用）。
//!
//! 用途：`tool_schema_snapshot` 等"只投影注册表、绝不执行"的路径，为
//! skill 工具 attach 无状态桩存储，使 schema 投影与真实会话一致（skill
//! 工具在真实装配中必然挂载，快照路径缺失会让 `REGISTERED_TOOL_NAMES`
//! 不含 skill，用户禁用 skill 的设置会在落盘求交时被静默过滤），同时不
//! 产生磁盘 / IndexedDB 副作用。也适用于需要轻量进程内 KV 的测试桩。
//!
//! TTL 语义：惰性过期（读路径过滤过期条目），`sweep_expired` 沿用默认
//! no-op（后台清理路径无需内存实现参与）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;

use super::kv::{KvStore, now_ms};
use crate::error::MemoryError;

/// 键 →（过期毫秒时间戳，JSON 载荷）。
type Entry = (Option<i64>, String);

/// 进程内 KV 桩：`HashMap` 后端，键 →（过期毫秒时间戳，JSON 载荷）。
#[derive(Clone, Default)]
pub struct InMemoryKvStore {
    inner: Arc<RwLock<HashMap<String, Entry>>>,
}

impl InMemoryKvStore {
    fn is_expired(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
        expires_at_ms.is_some_and(|at| at <= now_ms)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl KvStore for InMemoryKvStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
        let map = self
            .inner
            .read()
            .map_err(|e| MemoryError::Storage(format!("kv read lock: {e}")))?;
        let Some((expires_at_ms, json)) = map.get(key) else {
            return Ok(None);
        };
        if Self::is_expired(*expires_at_ms, now_ms()) {
            return Ok(None);
        }
        serde_json::from_str(json)
            .map(Some)
            .map_err(|e| MemoryError::Serialization(e.to_string()))
    }

    async fn set(
        &self,
        key: &str,
        value: &Value,
        ttl: Option<Duration>,
    ) -> Result<(), MemoryError> {
        let expires_at_ms = ttl.map(|d| {
            let millis = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
            now_ms().saturating_add(millis)
        });
        let mut map = self
            .inner
            .write()
            .map_err(|e| MemoryError::Storage(format!("kv write lock: {e}")))?;
        map.insert(key.to_string(), (expires_at_ms, value.to_string()));
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), MemoryError> {
        let mut map = self
            .inner
            .write()
            .map_err(|e| MemoryError::Storage(format!("kv write lock: {e}")))?;
        map.remove(key);
        Ok(())
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
        let now = now_ms();
        let map = self
            .inner
            .read()
            .map_err(|e| MemoryError::Storage(format!("kv read lock: {e}")))?;
        Ok(map
            .iter()
            .filter(|(key, (expires_at_ms, _))| {
                key.starts_with(prefix) && !Self::is_expired(*expires_at_ms, now)
            })
            .map(|(key, _)| key.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn in_memory_kv_set_get_delete_round_trip() {
        let kv = InMemoryKvStore::default();
        assert!(kv.get("a").await.unwrap().is_none());
        kv.set("a", &json!({"n": 1}), None).await.unwrap();
        assert_eq!(kv.get("a").await.unwrap().unwrap(), json!({"n": 1}));
        // 覆盖写
        kv.set("a", &json!({"n": 2}), None).await.unwrap();
        assert_eq!(kv.get("a").await.unwrap().unwrap(), json!({"n": 2}));
        kv.delete("a").await.unwrap();
        assert!(kv.get("a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn in_memory_kv_list_prefix_filters_expired_and_other_prefixes() {
        let kv = InMemoryKvStore::default();
        kv.set("p:x", &json!(1), None).await.unwrap();
        kv.set("p:y", &json!(2), Some(Duration::from_millis(1)))
            .await
            .unwrap();
        kv.set("q:z", &json!(3), None).await.unwrap();
        // TTL 1ms 立即过期，惰性清理：读路径（get/list）过滤，不残留
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(kv.get("p:y").await.unwrap().is_none());
        let keys = kv.list_prefix("p:").await.unwrap();
        assert_eq!(keys, vec!["p:x".to_string()]);
        let keys = kv.list_prefix("q:").await.unwrap();
        assert_eq!(keys, vec!["q:z".to_string()]);
    }
}
