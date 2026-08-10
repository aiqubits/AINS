//! Phase 0 Web Worker 冒烟测试（wasm32 + headless 浏览器）：验证
//! `WasmRuntimeAdapter` 在无 `Window` 的 DedicatedWorker 全局作用域下可用
//! （`sleep` 必须依赖全局 `setTimeout` 而非 `Window`），由 CI `wasm-pack test` 执行。

#![cfg(target_arch = "wasm32")]

use std::time::Duration;

use wasm_bindgen_test::*;

use rust_agent::WasmRuntimeAdapter;
use rust_agent::platform::Platform;
use rust_agent::runtime_adapter::RuntimeAdapter;

wasm_bindgen_test_configure!(run_in_dedicated_worker);

#[wasm_bindgen_test]
async fn sleep_works_without_window() {
    assert!(
        web_sys::window().is_none(),
        "expected no Window in a dedicated worker scope"
    );
    WasmRuntimeAdapter::sleep(Duration::from_millis(10)).await;
}

#[wasm_bindgen_test]
async fn spawn_works_in_worker() {
    let (tx, rx) = futures::channel::oneshot::channel::<u32>();
    WasmRuntimeAdapter::spawn(async move {
        let _ = tx.send(7);
    });
    assert_eq!(rx.await.unwrap(), 7);
    assert_eq!(Platform::current(), Platform::Web);
}
