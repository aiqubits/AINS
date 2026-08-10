//! Web 端 KvStore 实现：浏览器原生 IndexedDB（经 web-sys 绑定）。
//!
//! ObjectStore 命名与 Native redb 逻辑表一一对应（AINS_PLAN 4.1）；
//! 值为 [`Envelope`] bincode 字节（Uint8Array），与 Native 序列化格式一致。
//!
//! TTL 语义与 Native 相同：读时检查 + `sweep_expired` 后台清理。
//!
//! 持久化语义：IndexedDB 请求 onsuccess 先于事务提交触发，commit
//! 阶段仍可能失败（配额等）；写路径统一等待事务 complete/abort
//! 后才向调用方报告成功（引擎回滚逻辑依赖 `set` 成功即已落盘）。

#![cfg(target_arch = "wasm32")]

use std::time::Duration;

use js_sys::Uint8Array;
use serde_json::Value;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    IdbDatabase, IdbKeyRange, IdbObjectStore, IdbOpenDbRequest, IdbRequest, IdbTransaction,
    IdbTransactionMode,
};

use crate::error::MemoryError;
use crate::memory::kv::{ALL_TABLES, Envelope, KvStore, now_ms};

/// IndexedDB schema version. Version 1 contained only the legacy `kv` store;
/// version 2 adds the four Memory/Vector stores introduced with `MemoryStores`.
/// Keep this explicit so adding a new logical table always requires a storage
/// migration rather than silently leaving existing browser profiles behind.
const DB_SCHEMA_VERSION: u32 = 2;

fn storage_err(context: &str, e: impl std::fmt::Debug) -> MemoryError {
    MemoryError::Storage(format!("{context}: {e:?}"))
}

/// 将 IdbRequest 的 onsuccess/onerror 回调桥接为可 await 的 Future。
///
/// 单个 once 闭包同时挂 success/error 两个事件：request settle 时只触发
/// 其一，触发即消费释放闭包；若改为两个 `once_into_js` 闭包，未触发的
/// 那个每次请求都会泄漏一份 Rust 堆内存。
fn request_future(request: IdbRequest) -> JsFuture {
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let req = request.clone();
        let on_settled = Closure::once_into_js(move |event: web_sys::Event| {
            if event.type_() == "error" {
                let message = req
                    .error()
                    .ok()
                    .flatten()
                    .map(|e| e.message())
                    .unwrap_or_else(|| "IndexedDB request failed".into());
                let _ = reject.call1(&JsValue::UNDEFINED, &JsValue::from_str(&message));
            } else {
                let value = req.result().unwrap_or(JsValue::UNDEFINED);
                let _ = resolve.call1(&JsValue::UNDEFINED, &value);
            }
        });
        request.set_onsuccess(Some(on_settled.unchecked_ref()));
        request.set_onerror(Some(on_settled.unchecked_ref()));
    });
    JsFuture::from(promise)
}

/// 将 IdbTransaction 的 complete/abort 事件桥接为可 await 的 Future。
///
/// 请求 onsuccess 先于事务提交触发，commit 阶段仍可能失败（配额等）；
/// 写路径必须等事务 settle 才能报告持久化成功。complete 与 abort 恰好
/// 其一触发（error 事件后必随 abort，不单独挂），单个 once 闭包安全；
/// 需在发起请求前创建（同步挂好回调），未被 await 即丢弃时事件触发
/// 仅 settle 已丢弃的 promise，无泄漏。
fn transaction_future(tx: &IdbTransaction) -> JsFuture {
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let tx_ref = tx.clone();
        let on_settled = Closure::once_into_js(move |event: web_sys::Event| {
            if event.type_() == "complete" {
                let _ = resolve.call1(&JsValue::UNDEFINED, &JsValue::UNDEFINED);
            } else {
                let message = tx_ref
                    .error()
                    .map(|e| e.message())
                    .unwrap_or_else(|| "IndexedDB transaction aborted".into());
                let _ = reject.call1(&JsValue::UNDEFINED, &JsValue::from_str(&message));
            }
        });
        tx.set_oncomplete(Some(on_settled.unchecked_ref()));
        tx.set_onabort(Some(on_settled.unchecked_ref()));
    });
    JsFuture::from(promise)
}

/// 前缀 → IndexedDB 键范围（空前缀返回 None = 全表）。
///
/// 字符串键按 UTF-16 码元排序；上界取前缀的 code-unit 后继（排他），
/// 无后继（全 \u{FFFF}）时退化为 lower bound。后继可能少量过覆盖
/// （代理区跳跃），调用方必须再按 `starts_with` 过滤。
fn range_for_prefix(prefix: &str) -> Result<Option<IdbKeyRange>, MemoryError> {
    if prefix.is_empty() {
        return Ok(None);
    }
    let range = match crate::memory::kv::prefix_successor(prefix) {
        Some(upper) => IdbKeyRange::bound_with_lower_open_and_upper_open(
            &JsValue::from_str(prefix),
            &JsValue::from_str(&upper),
            false,
            true,
        ),
        None => IdbKeyRange::lower_bound(&JsValue::from_str(prefix)),
    }
    .map_err(|e| storage_err("key range", e))?;
    Ok(Some(range))
}

/// IndexedDB 数据库句柄（Web 存储后端入口）。
#[derive(Clone)]
pub struct IndexedDbBackend {
    db: IdbDatabase,
}

impl IndexedDbBackend {
    /// 打开（或创建）IndexedDB 数据库，并在 upgrade 回调中创建全部 ObjectStore。
    pub async fn open(db_name: &str) -> Result<Self, MemoryError> {
        let factory = web_sys::window()
            .ok_or_else(|| MemoryError::Storage("no window".into()))?
            .indexed_db()
            .map_err(|e| storage_err("indexedDB access", e))?
            .ok_or_else(|| MemoryError::Storage("indexedDB unavailable".into()))?;
        let open_req: IdbOpenDbRequest = factory
            // Do not keep this at v1: existing AINS browser profiles created
            // before MemoryStores have only `kv`, and IndexedDB only invokes
            // `onupgradeneeded` when the requested version increases.
            .open_with_u32(db_name, DB_SCHEMA_VERSION)
            .map_err(|e| storage_err("open", e))?;

        let req_clone = open_req.clone();
        // 持有而非 once_into_js：库已存在时 upgrade 不触发，
        // once_into_js 未触发即泄漏；open settle 后显式 drop 释放。
        let on_upgrade = Closure::once(move |_event: web_sys::Event| {
            if let Ok(result) = req_clone.result() {
                let db: IdbDatabase = result.unchecked_into();
                for name in ALL_TABLES {
                    // Existing stores make `create_object_store` return a
                    // ConstraintError; intentionally continue so a v1 → v2
                    // upgrade still creates every missing store.
                    let _ = db.create_object_store(name);
                }
            }
        });
        open_req.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));

        let db = request_future(open_req.into())
            .await
            .map_err(|e| storage_err("open db", e))?;
        // settle 后 upgradeneeded 不会再触发，安全释放
        drop(on_upgrade);
        Ok(Self {
            db: db.unchecked_into(),
        })
    }

    /// 获取绑定指定 ObjectStore 的 `KvStore` 句柄。
    pub fn store(&self, name: &str) -> IndexedDbKvStore {
        IndexedDbKvStore {
            db: self.db.clone(),
            store: name.to_string(),
        }
    }
}

/// 绑定单个 ObjectStore 的 IndexedDB `KvStore` 实现。
pub struct IndexedDbKvStore {
    db: IdbDatabase,
    store: String,
}

impl IndexedDbKvStore {
    /// 便捷构造：独立打开数据库并绑定 `kv` ObjectStore。
    pub async fn open(db_name: &str) -> Result<Self, MemoryError> {
        Ok(IndexedDbBackend::open(db_name)
            .await?
            .store(crate::memory::kv::TABLE_KV))
    }

    async fn read_raw(&self, key: &str) -> Result<Option<Envelope>, MemoryError> {
        let tx = self
            .db
            .transaction_with_str(&self.store)
            .map_err(|e| storage_err("ro transaction", e))?;
        let store = tx
            .object_store(&self.store)
            .map_err(|e| storage_err("object store", e))?;
        let req = store
            .get(&JsValue::from_str(key))
            .map_err(|e| storage_err("get", e))?;
        let value = request_future(req)
            .await
            .map_err(|e| storage_err("get request", e))?;
        if value.is_undefined() || value.is_null() {
            return Ok(None);
        }
        let bytes: Uint8Array = value.dyn_into().map_err(|e| storage_err("value type", e))?;
        Ok(Some(Envelope::decode(&bytes.to_vec())?))
    }

    async fn remove_raw(&self, key: &str) -> Result<(), MemoryError> {
        let tx = self
            .db
            .transaction_with_str_and_mode(&self.store, IdbTransactionMode::Readwrite)
            .map_err(|e| storage_err("rw transaction", e))?;
        let done = transaction_future(&tx);
        let store = tx
            .object_store(&self.store)
            .map_err(|e| storage_err("object store", e))?;
        let req = store
            .delete(&JsValue::from_str(key))
            .map_err(|e| storage_err("delete", e))?;
        request_future(req)
            .await
            .map_err(|e| storage_err("delete request", e))?;
        done.await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    /// 惰性过期删除：单个 rw 事务内 get → 复核 → delete，返回是否实际
    /// 删除。判定过期与删除之间的 await 点可能插入并发 `set` 刷新同
    /// key，无条件删除会丢新值。
    async fn remove_if_expired(&self, key: &str, now: i64) -> Result<bool, MemoryError> {
        let tx = self
            .db
            .transaction_with_str_and_mode(&self.store, IdbTransactionMode::Readwrite)
            .map_err(|e| storage_err("rw transaction", e))?;
        let done = transaction_future(&tx);
        let store = tx
            .object_store(&self.store)
            .map_err(|e| storage_err("object store", e))?;
        let removed = remove_expired_in_store(&store, key, now).await?;
        done.await.map_err(|e| storage_err("commit", e))?;
        Ok(removed)
    }

    /// 单个 ro 事务读取前缀命中的全部 (keys, values) JS 数组。
    async fn fetch_keys_values(
        &self,
        prefix: &str,
    ) -> Result<(js_sys::Array, js_sys::Array), MemoryError> {
        let tx = self
            .db
            .transaction_with_str(&self.store)
            .map_err(|e| storage_err("ro transaction", e))?;
        let store = tx
            .object_store(&self.store)
            .map_err(|e| storage_err("object store", e))?;
        let (keys_req, values_req): (IdbRequest, IdbRequest) = match range_for_prefix(prefix)? {
            None => (
                store
                    .get_all_keys()
                    .map_err(|e| storage_err("get_all_keys", e))?,
                store.get_all().map_err(|e| storage_err("get_all", e))?,
            ),
            // 上界可能少量过覆盖（代理区跳跃），循环内再按 starts_with 过滤。
            Some(range) => (
                store
                    .get_all_keys_with_key(&range)
                    .map_err(|e| storage_err("get_all_keys", e))?,
                store
                    .get_all_with_key(&range)
                    .map_err(|e| storage_err("get_all", e))?,
            ),
        };
        // 两个 Future 必须都先创建（同步挂好回调）再 await，
        // 否则 values 的 success 事件可能在挂回调前触发而永久挂起。
        let keys_fut = request_future(keys_req);
        let values_fut = request_future(values_req);
        let keys = keys_fut.await.map_err(|e| storage_err("keys request", e))?;
        let values = values_fut
            .await
            .map_err(|e| storage_err("values request", e))?;
        let keys: js_sys::Array = keys.dyn_into().map_err(|e| storage_err("keys type", e))?;
        let values: js_sys::Array = values
            .dyn_into()
            .map_err(|e| storage_err("values type", e))?;
        Ok((keys, values))
    }

    /// 单事务批量读取前缀命中的 (key, Envelope)（M2：避免 N+1 事务；
    /// M4：损坏行跳过、不自动删除）。
    async fn entries_with_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, Envelope)>, MemoryError> {
        let (keys, values) = self.fetch_keys_values(prefix).await?;
        let mut entries = Vec::with_capacity(keys.length() as usize);
        for (key, value) in keys.iter().zip(values.iter()) {
            let Some(key) = key.as_string() else { continue };
            // 上界过覆盖的非前缀键在此过滤
            if !key.starts_with(prefix) {
                continue;
            }
            let Ok(bytes) = value.dyn_into::<Uint8Array>() else {
                continue;
            };
            if let Ok(env) = Envelope::decode(&bytes.to_vec()) {
                entries.push((key, env));
            }
        }
        Ok(entries)
    }

    /// 单个 ro 事务读取前缀命中的全部键（不解码 Envelope；含过期与
    /// 损坏行，供 delete_prefix 等 clear 语义路径使用）。
    async fn fetch_keys(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
        let tx = self
            .db
            .transaction_with_str(&self.store)
            .map_err(|e| storage_err("ro transaction", e))?;
        let store = tx
            .object_store(&self.store)
            .map_err(|e| storage_err("object store", e))?;
        let req = match range_for_prefix(prefix)? {
            None => store
                .get_all_keys()
                .map_err(|e| storage_err("get_all_keys", e))?,
            Some(range) => store
                .get_all_keys_with_key(&range)
                .map_err(|e| storage_err("get_all_keys", e))?,
        };
        let keys = request_future(req)
            .await
            .map_err(|e| storage_err("keys request", e))?;
        let keys: js_sys::Array = keys.dyn_into().map_err(|e| storage_err("keys type", e))?;
        Ok(keys
            .iter()
            .filter_map(|k| k.as_string())
            // 上界过覆盖的非前缀键在此过滤
            .filter(|k| k.starts_with(prefix))
            .collect())
    }

    /// 全表扫描过期键：Envelope 解码后立即丢弃，仅收集过期键，Rust 侧
    /// 峰值内存从全表 (key, Envelope) 降为过期键列表（过期时间嵌在
    /// Envelope 内，JS 侧 get_all 物化不可避免；进一步流式化需游标，
    /// Phase 2 规模下暂不引入）。损坏行无法判定过期，跳过不删。
    async fn expired_keys(&self, now: i64) -> Result<Vec<String>, MemoryError> {
        let (keys, values) = self.fetch_keys_values("").await?;
        let mut expired = Vec::new();
        for (key, value) in keys.iter().zip(values.iter()) {
            let Some(key) = key.as_string() else { continue };
            let Ok(bytes) = value.dyn_into::<Uint8Array>() else {
                continue;
            };
            if Envelope::decode(&bytes.to_vec()).is_ok_and(|env| env.is_expired(now)) {
                expired.push(key);
            }
        }
        Ok(expired)
    }
}

/// 在已打开的 rw 事务内复核并删除过期键，返回是否实际删除。
///
/// 事务活性不变量：get / delete 均为 IDB 请求，await 在各自 success 事件
/// 的微任务内恢复，事务保持活跃；期间不得插入任何非 IDB 的 await 点
///（如 timer / fetch），否则事务自动提交，后续请求将报
/// TransactionInactiveError。
async fn remove_expired_in_store(
    store: &IdbObjectStore,
    key: &str,
    now: i64,
) -> Result<bool, MemoryError> {
    let get_req = store
        .get(&JsValue::from_str(key))
        .map_err(|e| storage_err("get", e))?;
    let value = request_future(get_req)
        .await
        .map_err(|e| storage_err("get request", e))?;
    let expired = value
        .dyn_into::<Uint8Array>()
        .ok()
        .and_then(|bytes| Envelope::decode(&bytes.to_vec()).ok())
        .is_some_and(|env| env.is_expired(now));
    if expired {
        let del_req = store
            .delete(&JsValue::from_str(key))
            .map_err(|e| storage_err("delete", e))?;
        request_future(del_req)
            .await
            .map_err(|e| storage_err("delete request", e))?;
    }
    Ok(expired)
}

#[async_trait::async_trait(?Send)]
impl KvStore for IndexedDbKvStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
        let now = now_ms();
        match self.read_raw(key).await? {
            None => Ok(None),
            Some(env) if env.is_expired(now) => {
                // 读时检查：惰性删除过期条目（同事务复核，避免误删
                // 并发刷新的新值）
                self.remove_if_expired(key, now).await?;
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
        let array = Uint8Array::from(bytes.as_slice());
        let tx = self
            .db
            .transaction_with_str_and_mode(&self.store, IdbTransactionMode::Readwrite)
            .map_err(|e| storage_err("rw transaction", e))?;
        let done = transaction_future(&tx);
        let store = tx
            .object_store(&self.store)
            .map_err(|e| storage_err("object store", e))?;
        let req = store
            .put_with_key(&array, &JsValue::from_str(key))
            .map_err(|e| storage_err("put", e))?;
        request_future(req)
            .await
            .map_err(|e| storage_err("put request", e))?;
        // 请求成功 ≠ 事务提交：等到 oncomplete 才能报告持久化成功
        done.await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), MemoryError> {
        self.remove_raw(key).await
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
        let now = now_ms();
        Ok(self
            .entries_with_prefix(prefix)
            .await?
            .into_iter()
            .filter(|(_, env)| !env.is_expired(now))
            .map(|(key, _)| key)
            .collect())
    }

    async fn sweep_expired(&self) -> Result<u64, MemoryError> {
        let now = now_ms();
        // 1 个 ro 扫描事务 + 1 个 rw 批量删除事务（原为每键一个 rw 事务
        // 的 N+1 开销）。扫描与删除之间可能有并发 set 刷新同 key：删除
        // 事务内逐键复核。中途失败整个删除事务回滚，本轮清理作废，
        // 由下一周期 sweep 重试。
        let candidates = self.expired_keys(now).await?;
        if candidates.is_empty() {
            return Ok(0);
        }
        let tx = self
            .db
            .transaction_with_str_and_mode(&self.store, IdbTransactionMode::Readwrite)
            .map_err(|e| storage_err("rw transaction", e))?;
        let done = transaction_future(&tx);
        let store = tx
            .object_store(&self.store)
            .map_err(|e| storage_err("object store", e))?;
        let mut removed = 0u64;
        for key in &candidates {
            if remove_expired_in_store(&store, key, now).await? {
                removed += 1;
            }
        }
        done.await.map_err(|e| storage_err("commit", e))?;
        Ok(removed)
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<u64, MemoryError> {
        // 1 个 ro 键扫描 + 1 个 rw 批量删除事务（默认实现为逐键 N+1 事务）。
        // 不能直接用 IdbKeyRange 整段 delete：上界可能过覆盖非前缀键
        // （代理区跳跃），需 starts_with 过滤后逐键排队删除，单事务提交。
        // 键扫描不解码 Envelope，过期与损坏行一并清除（clear 语义彻底）。
        let keys = self.fetch_keys(prefix).await?;
        if keys.is_empty() {
            return Ok(0);
        }
        let tx = self
            .db
            .transaction_with_str_and_mode(&self.store, IdbTransactionMode::Readwrite)
            .map_err(|e| storage_err("rw transaction", e))?;
        let done = transaction_future(&tx);
        let store = tx
            .object_store(&self.store)
            .map_err(|e| storage_err("object store", e))?;
        for key in &keys {
            // 请求仅排队；等待事务提交一次性生效（失败整体回滚）
            store
                .delete(&JsValue::from_str(key))
                .map_err(|e| storage_err("delete", e))?;
        }
        done.await.map_err(|e| storage_err("commit", e))?;
        Ok(keys.len() as u64)
    }
}
