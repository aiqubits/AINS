//! KV 存储统一抽象（短期记忆）。
//!
//! Native 端由 redb 实现（`kv_native.rs`），Web 端由浏览器 IndexedDB 实现
//! （`kv_web.rs`），两者在 Phase 2 落地；上层仅通过本 trait 使用，不感知后端。

use std::time::Duration;

use serde_json::Value;

use crate::error::MemoryError;
use crate::marker::MaybeSendSync;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait KvStore: MaybeSendSync {
    async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError>;

    /// 写入键值；`ttl` 为过期时长，`None` 表示永不过期。
    async fn set(&self, key: &str, value: &Value, ttl: Option<Duration>)
    -> Result<(), MemoryError>;

    async fn delete(&self, key: &str) -> Result<(), MemoryError>;

    /// 列出指定前缀的全部 key。
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError>;
}
