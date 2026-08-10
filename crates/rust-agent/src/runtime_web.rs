//! Web 端 RuntimeAdapter 实现（仅 `cfg(target_arch = "wasm32")` 编译）。

use std::future::Future;
use std::time::Duration;

use wasm_bindgen::{JsCast, JsValue};

use crate::marker::MaybeSend;
use crate::runtime_adapter::RuntimeAdapter;

pub struct WasmRuntimeAdapter;

impl RuntimeAdapter for WasmRuntimeAdapter {
    fn spawn<F>(future: F)
    where
        F: Future<Output = ()> + MaybeSend + 'static,
    {
        wasm_bindgen_futures::spawn_local(future);
    }

    async fn sleep(duration: Duration) {
        let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            // 取全局作用域的 setTimeout：Window 与 Web Worker 均可用，
            // 避免绑定 web_sys::window() 导致 Worker 内 panic。
            let global = js_sys::global();
            let set_timeout: js_sys::Function =
                js_sys::Reflect::get(&global, &JsValue::from_str("setTimeout"))
                    .expect("setTimeout missing on global scope")
                    .unchecked_into();
            set_timeout
                .call2(&global, &resolve, &JsValue::from(millis))
                .expect("setTimeout failed");
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}
