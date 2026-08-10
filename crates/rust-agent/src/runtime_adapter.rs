//! 平台异步 Runtime 抽象。
//!
//! 业务逻辑（kernel / services / context / policy）禁止直接使用
//! `tokio::spawn` / `wasm_bindgen_futures::spawn_local`，统一通过本 trait 调用；
//! 两个实现分别位于 cfg 门控的 `runtime_native.rs` / `runtime_web.rs`。

use std::future::Future;
use std::time::Duration;

use crate::marker::MaybeSend;

pub trait RuntimeAdapter {
    /// 在平台 Runtime 上派发异步任务。
    fn spawn<F>(future: F)
    where
        F: Future<Output = ()> + MaybeSend + 'static;

    /// 平台无阻塞休眠。
    fn sleep(duration: Duration) -> impl Future<Output = ()> + MaybeSend;
}
