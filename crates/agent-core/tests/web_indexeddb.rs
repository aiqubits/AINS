//! Phase 0.5：Web 端 IndexedDB 读写与持久化可行性验证。
//!
//! 仅在 CI 中通过 `wasm-pack test --headless --chrome` 执行
//! （IndexedDB 仅存在于浏览器环境，本地不假设存在浏览器/driver）。

#![cfg(target_arch = "wasm32")]

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

wasm_bindgen_test_configure!(run_in_browser);

const STORE: &str = "kv";

/// 将 IdbRequest 的 onsuccess/onerror 回调桥接为可 await 的 Future。
fn request_future(request: IdbRequest) -> JsFuture {
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let req = request.clone();
        let on_success = Closure::once_into_js(move |_event: web_sys::Event| {
            let value = req.result().unwrap_or(JsValue::UNDEFINED);
            let _ = resolve.call1(&JsValue::UNDEFINED, &value);
        });
        request.set_onsuccess(Some(on_success.unchecked_ref()));

        let req_err = request.clone();
        let on_error = Closure::once_into_js(move |_event: web_sys::Event| {
            let message = req_err
                .error()
                .ok()
                .flatten()
                .map(|e| e.message())
                .unwrap_or_else(|| "IndexedDB request failed".into());
            let _ = reject.call1(&JsValue::UNDEFINED, &JsValue::from_str(&message));
        });
        request.set_onerror(Some(on_error.unchecked_ref()));
    });
    JsFuture::from(promise)
}

async fn open_db(name: &str) -> IdbDatabase {
    let factory = web_sys::window()
        .expect("no window")
        .indexed_db()
        .expect("indexedDB access failed")
        .expect("indexedDB unavailable");
    let open_req: IdbOpenDbRequest = factory.open_with_u32(name, 1).expect("open failed");

    let req_clone = open_req.clone();
    let on_upgrade = Closure::once_into_js(move |_event: web_sys::Event| {
        let db: IdbDatabase = req_clone
            .result()
            .expect("no result during upgrade")
            .unchecked_into();
        let _ = db.create_object_store(STORE);
    });
    open_req.set_onupgradeneeded(Some(on_upgrade.unchecked_ref()));

    let db = request_future(open_req.into())
        .await
        .expect("open db failed");
    db.unchecked_into()
}

async fn put(db: &IdbDatabase, key: &str, value: &str) {
    let tx = db
        .transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite)
        .expect("rw transaction failed");
    let store = tx.object_store(STORE).expect("object store missing");
    let req = store
        .put_with_key(&JsValue::from_str(value), &JsValue::from_str(key))
        .expect("put failed");
    request_future(req).await.expect("put request failed");
}

async fn get(db: &IdbDatabase, key: &str) -> Option<String> {
    let tx = db
        .transaction_with_str(STORE)
        .expect("ro transaction failed");
    let store = tx.object_store(STORE).expect("object store missing");
    let req = store.get(&JsValue::from_str(key)).expect("get failed");
    let value = request_future(req).await.expect("get request failed");
    value.as_string()
}

#[wasm_bindgen_test]
async fn indexeddb_write_read_roundtrip() {
    let db = open_db("ains-phase0-roundtrip").await;
    put(&db, "k1", "hello-ains").await;
    assert_eq!(get(&db, "k1").await.as_deref(), Some("hello-ains"));
}

#[wasm_bindgen_test]
async fn indexeddb_persists_across_connections() {
    let db = open_db("ains-phase0-persist").await;
    put(&db, "k2", "durable-value").await;
    db.close();

    let db2 = open_db("ains-phase0-persist").await;
    assert_eq!(get(&db2, "k2").await.as_deref(), Some("durable-value"));
}

#[wasm_bindgen_test]
async fn indexeddb_missing_key_returns_none() {
    let db = open_db("ains-phase0-missing").await;
    assert_eq!(get(&db, "no-such-key").await, None);
}
