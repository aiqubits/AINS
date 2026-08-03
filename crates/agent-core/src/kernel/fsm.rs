//! FSM 合法转换表（对齐 AINS_PLAN 3.1 Agent Loop 状态图）。
//!
//! 事件循环在 debug 构建下逐次断言转换合法性；表本身是纯函数，双端通用。
//!
//! 外部 resume 入口：`prepare_continuation` 直接在循环外将 `Idle` 置为
//! `Querying`，因此表中不存在 `Idle → Querying` 边——该路径不经过
//! `debug_assert` 检查，是唯一允许的 FSM 表外转换。

use crate::kernel::state::StateKind;

/// 判定 `from → to` 是否为 Agent Loop 状态图中的合法转换。
///
/// 终态（Completed / Failed）无出边；`Compacting → Querying` 为压缩成功
/// 主出边（Phase 5.5 已落地），`Compacting → Idle` 为 PreCompact hook 阻断
/// 或未发生压缩时的回退出口。`ExecutingTools → Idle` 为用户中断出边
/// （Phase 7.1：工具批边界检测到 Interrupt 时中止查询回 Idle）。
pub fn is_valid_transition(from: StateKind, to: StateKind) -> bool {
    use StateKind::*;
    match from {
        Idle => matches!(to, Observing | Waiting | Completed),
        Observing => matches!(to, Querying | Idle | Completed),
        Querying => matches!(to, Idle | ExecutingTools | Compacting | Failed),
        ExecutingTools => matches!(to, Querying | Idle | Failed),
        Compacting => matches!(to, Querying | Idle | Failed),
        Waiting => matches!(to, Idle),
        Completed | Failed => false,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use StateKind::*;

    #[test]
    fn tool_loop_path_is_valid() {
        for (from, to) in [
            (Idle, Observing),
            (Observing, Querying),
            (Querying, ExecutingTools),
            (ExecutingTools, Querying),
            (Querying, Idle),
        ] {
            assert!(is_valid_transition(from, to), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn idle_wait_shutdown_paths_are_valid() {
        for (from, to) in [
            (Idle, Waiting),
            (Waiting, Idle),
            (Idle, Completed),
            (Observing, Completed),
            (Querying, Compacting),
            (Compacting, Querying),
            (ExecutingTools, Failed),
            (Querying, Failed),
            // 用户中断出边（Phase 7.1）
            (ExecutingTools, Idle),
        ] {
            assert!(is_valid_transition(from, to), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn terminal_states_have_no_outgoing_edges() {
        for to in [
            Idle,
            Observing,
            Querying,
            ExecutingTools,
            Compacting,
            Waiting,
            Completed,
            Failed,
        ] {
            assert!(!is_valid_transition(Completed, to));
            assert!(!is_valid_transition(Failed, to));
        }
    }

    #[test]
    fn invalid_shortcuts_are_rejected() {
        for (from, to) in [
            (Idle, Querying),
            (Idle, ExecutingTools),
            (Waiting, Observing),
            (Observing, ExecutingTools),
        ] {
            assert!(!is_valid_transition(from, to), "{from:?} -> {to:?}");
        }
    }
}
