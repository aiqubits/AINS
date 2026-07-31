//! Agent Runtime 桥接层（Phase 6.2，Native）。
//!
//! 三个子模块经 `#[path]` 直接复用 `app/web/src/agent/` 的实现，确保
//! Web/Desktop 桥接语义与测试完全一致（平台差异集中在 `service.rs` 的
//! `cfg` 分支）。

#[path = "../../web/src/agent/permission_bridge.rs"]
pub mod permission_bridge;
#[path = "../../web/src/agent/service.rs"]
pub mod service;
#[path = "../../web/src/agent/view_model.rs"]
pub mod view_model;
