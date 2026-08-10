use dioxus::prelude::*;

use crate::{EN, I18nContext};

/// Agent 运行状态（视图模型，宿主从流事件派生；ui 不依赖 rust-agent）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentStatusView {
    #[default]
    Idle,
    Thinking,
    RunningTools,
    Compacting,
    Error,
}

impl AgentStatusView {
    fn dot_class(self) -> &'static str {
        match self {
            Self::Idle => "ains-agent-status__dot--idle",
            Self::Thinking => "ains-agent-status__dot--thinking",
            Self::RunningTools => "ains-agent-status__dot--running",
            Self::Compacting => "ains-agent-status__dot--compacting",
            Self::Error => "ains-agent-status__dot--error",
        }
    }
}

/// Agent 状态指示器（Phase 6.5）：圆点 + 文案，反映当前 FSM/流状态。
#[component]
pub fn AgentStatus(status: AgentStatusView) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    let label = match status {
        AgentStatusView::Idle => t.agent_status_idle,
        AgentStatusView::Thinking => t.agent_status_thinking,
        AgentStatusView::RunningTools => t.agent_status_running_tools,
        AgentStatusView::Compacting => t.agent_status_compacting,
        AgentStatusView::Error => t.agent_status_error,
    };
    let pulsing = matches!(
        status,
        AgentStatusView::Thinking | AgentStatusView::RunningTools | AgentStatusView::Compacting
    );

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/agent_status.css") }
        span { class: "ains-agent-status",
            span {
                class: if pulsing {
                    format!("ains-agent-status__dot {} ains-agent-status__dot--pulse", status.dot_class())
                } else {
                    format!("ains-agent-status__dot {}", status.dot_class())
                },
            }
            span { class: "ains-agent-status__label", {label} }
        }
    }
}
