//! 跨平台 Send/Sync 标记。
//!
//! Native 端异步任务要求 `Send`（tokio 多线程调度），WASM 端单线程且
//! JS 互操作类型不满足 `Send`。业务逻辑统一以 `MaybeSend` / `MaybeSendSync`
//! 作为边界，由 cfg 在两个 target 上收敛为不同的实际约束。

#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}

#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync + ?Sized> MaybeSendSync for T {}

#[cfg(target_arch = "wasm32")]
pub trait MaybeSendSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSendSync for T {}
