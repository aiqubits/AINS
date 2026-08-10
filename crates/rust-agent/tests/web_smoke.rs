//! Phase 0 Web 端冒烟测试（wasm32 + headless 浏览器）：验证 WASM 运行时
//! 适配器与核心类型在浏览器环境下的基础行为，由 CI `wasm-pack test` 执行。

#![cfg(target_arch = "wasm32")]

use std::time::Duration;

use serde_json::json;
use wasm_bindgen_test::*;

use rust_agent::WasmRuntimeAdapter;
use rust_agent::kernel::messages::{ContentBlock, ConversationMessage, Role};
use rust_agent::model_client::ModelRequest;
use rust_agent::platform::Platform;
use rust_agent::runtime_adapter::RuntimeAdapter;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn platform_current_is_web() {
    assert_eq!(Platform::current(), Platform::Web);
}

#[wasm_bindgen_test]
fn message_serde_roundtrip_on_wasm() {
    let message = ConversationMessage {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "hello wasm".into(),
        }],
    };
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(value["role"], "user");
    assert_eq!(value["content"][0]["type"], "text");
    let back: ConversationMessage = serde_json::from_value(value).unwrap();
    assert_eq!(back, message);
}

#[wasm_bindgen_test]
fn model_request_default_on_wasm() {
    assert_eq!(ModelRequest::default().max_output_tokens, 4096);
    assert_eq!(json!({"ok": true})["ok"], json!(true));
}

#[wasm_bindgen_test]
async fn wasm_sleep_completes() {
    WasmRuntimeAdapter::sleep(Duration::from_millis(10)).await;
}

#[wasm_bindgen_test]
async fn wasm_spawn_runs_task() {
    let (tx, rx) = futures::channel::oneshot::channel::<u32>();
    WasmRuntimeAdapter::spawn(async move {
        let _ = tx.send(42);
    });
    assert_eq!(rx.await.unwrap(), 42);
}
