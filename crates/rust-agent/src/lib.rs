//! AINS Agent Runtime Core.
//!
//! 单一 crate、WASM/Native 双编译目标的 Embedded Agent Runtime（见 AINS_PLAN 第二章）。
//! 业务逻辑只依赖 trait 抽象；平台特定实现（tokio / wasm-bindgen / redb / IndexedDB）
//! 收敛在 cfg 门控的适配文件中，按 target 互斥编译。

pub mod commands;
pub mod context;
pub mod error;
pub mod fnmatch;
pub mod hooks;
pub mod kernel;
pub mod marker;
pub mod memory;
pub mod model_client;
pub mod model_service;
pub mod perception;
pub mod personalization;
pub mod platform;
pub mod plugins;
pub mod policy;
pub mod runtime_adapter;
pub mod skills;
pub mod swarm;
pub mod tools;

// 后台任务（7+.4）依赖子进程 / tokio，Native 先行（Web 无子进程模型）。
#[cfg(not(target_arch = "wasm32"))]
pub mod tasks;

#[cfg(not(target_arch = "wasm32"))]
mod runtime_native;
#[cfg(not(target_arch = "wasm32"))]
pub use runtime_native::TokioRuntimeAdapter;

#[cfg(target_arch = "wasm32")]
mod runtime_web;
#[cfg(target_arch = "wasm32")]
pub use runtime_web::WasmRuntimeAdapter;
