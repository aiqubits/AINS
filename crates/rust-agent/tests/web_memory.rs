//! Phase 2 记忆系统集成测试（Web / IndexedDB 后端）。
//!
//! 仅在 CI 中通过 `wasm-pack test --headless --chrome` 执行；
//! 覆盖与 `tests/memory_native.rs` 相同的行为契约（KvStore + TTL、
//! 线性向量索引、MemoryEngine、memdir）。

#![cfg(target_arch = "wasm32")]

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest};

use rust_agent::WasmRuntimeAdapter;
use rust_agent::memory::{
    DefaultVectorIndexManager, EncryptedKvStore, EncryptionKey, IndexedDbBackend, KvStore,
    MemdirStore, MemoryEngine, MemoryNamespace, Metric, NewMemoryEntry, TABLE_EMBEDDINGS,
    TABLE_HNSW_CACHE, TABLE_KV, TABLE_MEMORIES, VectorIndexConfig, VectorIndexManager,
    format_iso_utc, now_ms, spawn_ttl_sweeper,
};

wasm_bindgen_test_configure!(run_in_browser);

const DIM: u32 = 8;

fn embed_text(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM as usize];
    for c in text.chars() {
        v[(c as usize) % DIM as usize] += 1.0;
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn test_config() -> VectorIndexConfig {
    VectorIndexConfig {
        dimension: DIM,
        distance_metric: Metric::Cosine,
        m: 16,
        ef: 50,
    }
}

async fn open_backend(db_name: &str) -> IndexedDbBackend {
    IndexedDbBackend::open(db_name).await.expect("open idb")
}

/// Create the database layout shipped before `MemoryStores`: schema version 1
/// contains only the legacy generic KV object store. This lets the upgrade test
/// exercise a real IndexedDB versionchange instead of merely opening a fresh
/// v2 database.
async fn create_legacy_v1_kv_only_database(db_name: &str) -> IdbDatabase {
    let factory = web_sys::window()
        .expect("no window")
        .indexed_db()
        .expect("indexedDB access")
        .expect("indexedDB unavailable");
    let open_req: IdbOpenDbRequest = factory
        .open_with_u32(db_name, 1)
        .expect("open legacy v1 database");

    let request = open_req.clone();
    let on_upgrade = Closure::once_into_js(move |_event: web_sys::Event| {
        let db: IdbDatabase = request
            .result()
            .expect("legacy upgrade result")
            .unchecked_into();
        db.create_object_store(TABLE_KV)
            .expect("create legacy kv store");
    });
    open_req.set_onupgradeneeded(Some(on_upgrade.unchecked_ref()));

    let request: IdbRequest = open_req.into();
    let opened = js_sys::Promise::new(&mut |resolve, reject| {
        let request = request.clone();
        let request_for_callback = request.clone();
        let on_settled = Closure::once_into_js(move |event: web_sys::Event| {
            if event.type_() == "error" {
                let _ = reject.call1(
                    &JsValue::UNDEFINED,
                    &JsValue::from_str("legacy open failed"),
                );
            } else {
                let value = request_for_callback.result().unwrap_or(JsValue::UNDEFINED);
                let _ = resolve.call1(&JsValue::UNDEFINED, &value);
            }
        });
        request.set_onsuccess(Some(on_settled.unchecked_ref()));
        request.set_onerror(Some(on_settled.unchecked_ref()));
    });
    JsFuture::from(opened)
        .await
        .expect("open legacy v1 database")
        .unchecked_into()
}

#[wasm_bindgen_test]
async fn existing_v1_database_is_upgraded_with_all_memory_stores() {
    let db_name = format!("ains-p2-v1-upgrade-{}", now_ms());
    let legacy = create_legacy_v1_kv_only_database(&db_name).await;
    legacy.close();

    let backend = open_backend(&db_name).await;
    for table in [
        TABLE_KV,
        TABLE_MEMORIES,
        TABLE_EMBEDDINGS,
        rust_agent::memory::TABLE_DOCUMENTS,
        TABLE_HNSW_CACHE,
    ] {
        let store = backend.store(table);
        store
            .set("migration/probe", &json!(table), None)
            .await
            .expect("upgraded store must be writable");
        assert_eq!(
            store
                .get("migration/probe")
                .await
                .expect("upgraded store readable"),
            Some(json!(table)),
            "store {table} was not created during v1 upgrade"
        );
    }
}

async fn build_engine(backend: &IndexedDbBackend, namespace: MemoryNamespace) -> MemoryEngine {
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.store(TABLE_EMBEDDINGS));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.store(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    manager
        .create_index(namespace, test_config())
        .await
        .expect("create index");
    MemoryEngine::new(
        Arc::new(backend.store(TABLE_MEMORIES)),
        embeddings,
        Box::new(manager),
    )
}

async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// 改写 memdir 条目文件里的时间戳（frontmatter 为明文，逐行替换）。
fn rewrite_entry_timestamps(raw: &str, created: &str, updated: &str) -> String {
    raw.lines()
        .map(|line| {
            if line.starts_with("created_at: ") {
                format!("created_at: {created}")
            } else if line.starts_with("updated_at: ") {
                format!("updated_at: {updated}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[wasm_bindgen_test]
async fn kv_roundtrip_delete_and_prefix() {
    let kv = open_backend("ains-p2-kv").await.store(TABLE_KV);

    kv.set("a/1", &json!({"v": 1}), None).await.unwrap();
    kv.set("a/2", &json!("two"), None).await.unwrap();
    kv.set("b/1", &json!(3), None).await.unwrap();

    assert_eq!(kv.get("a/1").await.unwrap(), Some(json!({"v": 1})));
    assert_eq!(kv.get("missing").await.unwrap(), None);

    let mut keys = kv.list_prefix("a/").await.unwrap();
    keys.sort();
    assert_eq!(keys, vec!["a/1".to_string(), "a/2".to_string()]);

    kv.delete("a/1").await.unwrap();
    assert_eq!(kv.get("a/1").await.unwrap(), None);
}

#[wasm_bindgen_test]
async fn kv_ttl_lazy_expiry_and_sweep() {
    let kv = open_backend("ains-p2-ttl").await.store(TABLE_KV);

    kv.set("ttl/gone", &json!("x"), Some(Duration::from_millis(10)))
        .await
        .unwrap();
    kv.set("ttl/kept", &json!("y"), Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    sleep_ms(30).await;

    assert_eq!(kv.get("ttl/gone").await.unwrap(), None);
    assert_eq!(kv.get("ttl/kept").await.unwrap(), Some(json!("y")));

    kv.set("ttl/gone2", &json!("z"), Some(Duration::from_millis(1)))
        .await
        .unwrap();
    sleep_ms(10).await;
    assert!(kv.sweep_expired().await.unwrap() >= 1);
    assert_eq!(kv.list_prefix("ttl/").await.unwrap(), vec!["ttl/kept"]);
}

// ── Review 二轮回归：后台 TTL sweeper 在 Wasm Runtime 上同样派发/停止（对齐 Native 契约）──

#[wasm_bindgen_test]
async fn ttl_sweeper_runs_and_stops() {
    let kv: Arc<dyn KvStore> = Arc::new(open_backend("ains-p2-sweeper").await.store(TABLE_KV));
    kv.set("s/1", &json!("v"), Some(Duration::from_millis(5)))
        .await
        .unwrap();

    let handle =
        spawn_ttl_sweeper::<WasmRuntimeAdapter>(vec![Arc::clone(&kv)], Duration::from_millis(20));
    sleep_ms(80).await;
    assert!(kv.list_prefix("s/").await.unwrap().is_empty());

    handle.stop();
    assert!(handle.is_stopped());
}

#[wasm_bindgen_test]
async fn vector_search_returns_closest_and_survives_reload() {
    let backend = open_backend("ains-p2-vector").await;
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let texts = [
        "rust borrow checker",
        "tokio async runtime",
        "coffee brewing",
    ];
    for text in texts {
        engine
            .remember(
                MemoryNamespace::Personal,
                text,
                &embed_text(text),
                json!({}),
            )
            .await
            .unwrap();
    }

    let hits = engine
        .search(
            MemoryNamespace::Personal,
            &embed_text("rust borrow checker"),
            1,
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.content, "rust borrow checker");
    assert!(hits[0].1 > 0.9);

    // write-through：新 engine 从 embeddings 重新加载线性索引
    drop(engine);
    let engine2 = build_engine(&backend, MemoryNamespace::Personal).await;
    assert_eq!(engine2.count(MemoryNamespace::Personal).await.unwrap(), 3);
    let hits = engine2
        .search(MemoryNamespace::Personal, &embed_text("coffee brewing"), 1)
        .await
        .unwrap();
    assert_eq!(hits[0].0.content, "coffee brewing");
}

#[wasm_bindgen_test]
async fn unreadable_encrypted_embedding_does_not_disable_valid_recall() {
    let backend = open_backend("ains-memory-encrypted-row-tolerance").await;
    let raw_embeddings: Arc<dyn KvStore> = Arc::new(backend.store(TABLE_EMBEDDINGS));
    // WASM executes this test on one browser thread. `KvStore` deliberately
    // relaxes Send/Sync there, while the production MemoryEngine API still
    // owns stores through Arc for cross-target parity.
    #[allow(clippy::arc_with_non_send_sync)]
    let embeddings: Arc<dyn KvStore> = Arc::new(EncryptedKvStore::with_table_domain(
        Arc::clone(&raw_embeddings),
        EncryptionKey::from_bytes([7u8; 32]),
        TABLE_EMBEDDINGS,
    ));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.store(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    manager
        .create_index(MemoryNamespace::Personal, test_config())
        .await
        .unwrap();
    let mut engine = MemoryEngine::new(
        Arc::new(backend.store(TABLE_MEMORIES)),
        Arc::clone(&embeddings),
        Box::new(manager),
    );
    let vector = embed_text("valid encrypted recall fact");
    engine
        .remember(
            MemoryNamespace::Personal,
            "valid encrypted recall fact",
            &vector,
            json!({}),
        )
        .await
        .unwrap();

    // Write malformed raw data outside the wrapper. Loading must skip only
    // this row and preserve the valid encrypted embedding.
    raw_embeddings
        .set("personal/tampered", &json!({ "not": "sealed" }), None)
        .await
        .unwrap();

    let hits = engine
        .search(MemoryNamespace::Personal, &vector, 5)
        .await
        .unwrap();
    assert!(
        hits.iter()
            .any(|(entry, _)| entry.content == "valid encrypted recall fact")
    );
}

#[wasm_bindgen_test]
async fn engine_dedupes_and_forgets() {
    let backend = open_backend("ains-p2-dedupe").await;
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let first = engine
        .remember(
            MemoryNamespace::Personal,
            "User prefers dark mode.",
            &embed_text("User prefers dark mode."),
            json!({"importance": 1.0}),
        )
        .await
        .unwrap();
    let second = engine
        .remember(
            MemoryNamespace::Personal,
            "user prefers DARK mode",
            &embed_text("user prefers DARK mode"),
            json!({"importance": 2.5}),
        )
        .await
        .unwrap();

    assert_eq!(first.id, second.id);
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);

    engine
        .forget(MemoryNamespace::Personal, &first.id)
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 0);
}

#[wasm_bindgen_test]
async fn memdir_add_scan_remove_on_indexeddb() {
    let kv: Arc<dyn KvStore> = Arc::new(open_backend("ains-p2-memdir").await.store(TABLE_KV));
    let store = MemdirStore::new(Arc::clone(&kv));

    let prompt = store.load_memory_prompt().await.unwrap();
    assert!(prompt.contains("## Durable memory policy"));

    let filename = store
        .add_entry(NewMemoryEntry {
            title: "Web Build".into(),
            body: "Use wasm-pack for the web target.".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(filename, "web_build.md");

    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "Web Build");

    assert!(store.remove_entry("web_build").await.unwrap());
    assert!(store.scan(10).await.unwrap().is_empty());
}

// ── Fix 回归：IndexedDB 前缀上界（UTF-16 code-unit 后继）──

#[wasm_bindgen_test]
async fn list_prefix_covers_high_code_unit_suffixes() {
    let kv = open_backend("ains-p2-prefix").await.store(TABLE_KV);

    // 旧上界 `prefix+U+10FFFF`（代理对）在 UTF-16 序下会漏掉
    // 后继字符落在 [U+E000, U+FFFF] 的键
    kv.set("p/ascii", &json!(1), None).await.unwrap();
    kv.set("p/\u{E000}", &json!(2), None).await.unwrap();
    kv.set("p/\u{FFFF}", &json!(3), None).await.unwrap();
    kv.set("p/\u{10FFFF}", &json!(4), None).await.unwrap();
    kv.set("q/other", &json!(5), None).await.unwrap();

    let mut keys = kv.list_prefix("p/").await.unwrap();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "p/ascii".to_string(),
            "p/\u{E000}".to_string(),
            "p/\u{FFFF}".to_string(),
            "p/\u{10FFFF}".to_string(),
        ]
    );
}

// ── Review 四轮回归：批量 sweep（1 ro 扫描 + 1 rw 删除事务）规模化正确性 ──

#[wasm_bindgen_test]
async fn sweep_expired_at_scale_removes_only_expired() {
    let kv = open_backend("ains-p2-sweep-scale").await.store(TABLE_KV);

    // 150 条过期 + 50 条存活（混合写入，验证删除事务内逐键复核
    // 不误删存活条目）
    for i in 0..200 {
        let (key, ttl) = if i % 4 == 3 {
            (format!("scale/live-{i:03}"), Duration::from_secs(3600))
        } else {
            (format!("scale/gone-{i:03}"), Duration::from_millis(1))
        };
        kv.set(&key, &json!(i), Some(ttl)).await.unwrap();
    }
    sleep_ms(20).await;

    assert_eq!(kv.sweep_expired().await.unwrap(), 150);
    let survivors = kv.list_prefix("scale/").await.unwrap();
    assert_eq!(survivors.len(), 50);
    assert!(survivors.iter().all(|k| k.starts_with("scale/live-")));

    // 幂等：再扫无可删
    assert_eq!(kv.sweep_expired().await.unwrap(), 0);
}

// ── Review 四轮回归：memdir TTL 锚定 updated_at，刷新复活（镜像 Native 用例，
// 双端契约一致）──

#[wasm_bindgen_test]
async fn memdir_ttl_anchors_to_updated_at_on_indexeddb() {
    let kv: Arc<dyn KvStore> = Arc::new(open_backend("ains-p2-memdir-ttl").await.store(TABLE_KV));
    let store = MemdirStore::new(Arc::clone(&kv));

    let body = "Use blue-green deploys for the API.";
    let filename = store
        .add_entry(NewMemoryEntry {
            title: "Deploy Notes".into(),
            body: body.into(),
            ttl_days: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    let key = format!("memdir/entries/{filename}");

    let ten_days_ago = format_iso_utc(now_ms() - 10 * 24 * 3600 * 1000);
    let fresh = format_iso_utc(now_ms());

    // created_at 超出 TTL、updated_at 新鲜：锚定 updated_at 应仍可见
    let raw = kv.get(&key).await.unwrap().unwrap();
    let text = rewrite_entry_timestamps(raw.as_str().unwrap(), &ten_days_ago, &fresh);
    kv.set(&key, &serde_json::Value::String(text), None)
        .await
        .unwrap();
    assert_eq!(store.scan(10).await.unwrap().len(), 1);

    // 双时间戳都超出 TTL → 不可见
    let raw = kv.get(&key).await.unwrap().unwrap();
    let text = rewrite_entry_timestamps(raw.as_str().unwrap(), &ten_days_ago, &ten_days_ago);
    kv.set(&key, &serde_json::Value::String(text), None)
        .await
        .unwrap();
    assert!(store.scan(10).await.unwrap().is_empty());

    // 同签名 add_entry 刷新 updated_at → 复活
    let dup = store
        .add_entry(NewMemoryEntry {
            title: "Deploy Notes".into(),
            body: body.into(),
            ttl_days: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(dup, filename);
    assert_eq!(store.scan(10).await.unwrap().len(), 1);
}

// ── Fix 回归：search 回填跳过损坏 memories 行，不毒化整次检索 ──

#[wasm_bindgen_test]
async fn search_skips_corrupt_memory_rows() {
    let backend = open_backend("ains-p2-corrupt-search").await;
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let mut ids = Vec::new();
    for text in ["alpha fact", "gamma idea"] {
        let entry = engine
            .remember(
                MemoryNamespace::Personal,
                text,
                &embed_text(text),
                json!({}),
            )
            .await
            .unwrap();
        ids.push(entry.id);
    }

    // JSON 级损坏：Envelope 合法但内容不是 MemoryEntry
    let memories = backend.store(TABLE_MEMORIES);
    memories
        .set(
            &format!("personal/{}", ids[0]),
            &json!(["not", "an", "entry"]),
            None,
        )
        .await
        .unwrap();

    // 旧行为：损坏行被命中即整次 search 报 Serialization 错误
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text("gamma idea"), 2)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.content, "gamma idea");
}

// ── Fix 回归：线性索引 remove 压缩 + 加载韧性 ──

#[wasm_bindgen_test]
async fn linear_index_remove_compacts_and_load_skips_corrupt_rows() {
    let backend = open_backend("ains-p2-linear").await;
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let texts = [
        "rust borrow checker",
        "tokio async runtime",
        "coffee brewing",
    ];
    let mut ids = Vec::new();
    for text in texts {
        let entry = engine
            .remember(
                MemoryNamespace::Personal,
                text,
                &embed_text(text),
                json!({}),
            )
            .await
            .unwrap();
        ids.push(entry.id);
    }

    // remove 后（swap_remove 压缩）其余条目仍可检索
    engine
        .forget(MemoryNamespace::Personal, &ids[0])
        .await
        .unwrap();
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text("coffee brewing"), 3)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.content, "coffee brewing");

    // 注入损坏 embedding 行后重建：跳过坏行，有效数据可检索
    let embeddings = backend.store(TABLE_EMBEDDINGS);
    embeddings
        .set("personal/bad-dim", &json!([1.0, 2.0]), None)
        .await
        .unwrap();
    drop(engine);
    let engine2 = build_engine(&backend, MemoryNamespace::Personal).await;
    let hits = engine2
        .search(
            MemoryNamespace::Personal,
            &embed_text("tokio async runtime"),
            2,
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.content, "tokio async runtime");
}

// ── Review 五轮回归：写路径等待事务提交（请求成功 ≠ 提交），
// set/delete 返回 Ok 即已落盘，重新打开后可见 ──

#[wasm_bindgen_test]
async fn writes_persist_across_reopen() {
    let db_name = "ains-p2-commit";
    {
        let kv = open_backend(db_name).await.store(TABLE_KV);
        for i in 0..50 {
            kv.set(&format!("c/{i:02}"), &json!(i), None).await.unwrap();
        }
        kv.delete("c/00").await.unwrap();
    }

    let kv = open_backend(db_name).await.store(TABLE_KV);
    assert_eq!(kv.get("c/00").await.unwrap(), None);
    assert_eq!(kv.get("c/49").await.unwrap(), Some(json!(49)));
    assert_eq!(kv.list_prefix("c/").await.unwrap().len(), 49);
}

// ── Review 五轮回归：delete_prefix 单事务批量删除；上界过覆盖区间内的
// 非前缀键（代理区跳跃）不被误删，过期行一并清除 ──

#[wasm_bindgen_test]
async fn delete_prefix_spares_lookalike_keys() {
    let kv = open_backend("ains-p2-delete-prefix").await.store(TABLE_KV);

    kv.set("p\u{D7FF}a", &json!(1), None).await.unwrap();
    kv.set("p\u{D7FF}gone", &json!(2), Some(Duration::from_millis(1)))
        .await
        .unwrap();
    // 落在 [p\u{D7FF}, p\u{E000}) 过覆盖区间内但不属于前缀：
    // 直接按 IdbKeyRange 整段删除会误伤此键
    kv.set("p\u{10000}", &json!(3), None).await.unwrap();
    sleep_ms(10).await;

    // 前缀内存活 + 过期行一并清除（键扫描不解码 Envelope）
    assert_eq!(kv.delete_prefix("p\u{D7FF}").await.unwrap(), 2);
    assert!(kv.list_prefix("p\u{D7FF}").await.unwrap().is_empty());
    assert_eq!(kv.get("p\u{10000}").await.unwrap(), Some(json!(3)));
    // 幂等：再删无可删
    assert_eq!(kv.delete_prefix("p\u{D7FF}").await.unwrap(), 0);
}
