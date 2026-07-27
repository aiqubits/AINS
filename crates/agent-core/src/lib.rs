//! AINS Agent Runtime Core.
//!
//! 单一 crate、WASM/Native 双编译目标的 Embedded Agent Runtime（见 AINS_PLAN 第二章）。
//! 业务逻辑只依赖 trait 抽象；平台特定实现（tokio / wasm-bindgen / redb / IndexedDB）
//! 收敛在 cfg 门控的适配文件中，按 target 互斥编译。

pub mod error;
pub mod kernel;
pub mod marker;
pub mod memory;
pub mod model_client;
pub mod platform;
pub mod runtime_adapter;
pub mod skills;
pub mod tools;

#[cfg(not(target_arch = "wasm32"))]
mod runtime_native;
#[cfg(not(target_arch = "wasm32"))]
pub use runtime_native::TokioRuntimeAdapter;

#[cfg(target_arch = "wasm32")]
mod runtime_web;
#[cfg(target_arch = "wasm32")]
pub use runtime_web::WasmRuntimeAdapter;
