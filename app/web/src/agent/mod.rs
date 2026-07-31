//! Agent Runtime 桥接层（Phase 6.1/6.11）。
//!
//! - `service`：AgentKernel / ToolRuntime / 存储装配（平台差异集中于此）
//! - `view_model`：`StreamEvent` → Chat 视图模型纯函数映射
//! - `permission_bridge`：权限确认 / ask_user_question 的 channel 桥接
//!
//! desktop 端经 `#[path]` 引用这三个文件复用同一实现与测试。

pub mod permission_bridge;
pub mod service;
pub mod view_model;
