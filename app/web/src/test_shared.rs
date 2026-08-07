//! 测试共享设施（仅测试构建）：进程级全局状态（ui crate 的
//! PERSIST_ERROR / TOOL_STATE_LOAD_ERROR 信号与 PERSIST_STATE 状态机）
//! 跨测试存活且被多个测试模块（views::tools / agent::service）读写，
//! cargo test 并行执行时交叉污染会导致偶发断言失败——凡读写这些全局
//! 状态的测试必须持有 [`SIGNAL_TEST_LOCK`] 串行执行。
//!
//! 独立成文件供 web / desktop 两个 crate 复用：desktop 经 `#[path]`
//! 直接包含 `web/src/views/tools.rs` 与 `web/src/agent/service.rs`，
//! 其中的测试引用 `crate::test_shared`，故 desktop crate 根也必须提供
//! 同名模块（见 `app/desktop/src/main.rs`）。
pub static SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
