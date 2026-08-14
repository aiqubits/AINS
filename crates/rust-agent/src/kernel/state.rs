//! FSM 状态与事件定义（对齐 AINS_PLAN 3.3；状态集合直接映射
//! Harness `engine/query.py` 流式工具循环的各阶段）。

use serde::{Deserialize, Serialize};

use crate::error::AgentError;
use crate::kernel::messages::ToolUse;

/// 压缩触发方式（对齐基线 CompactProgressEvent 的 trigger 三态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Auto,
    Manual,
    Reactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemEventType {
    Startup,
    Shutdown,
}

/// 用户消息附件；`image/*` 在上下文构建时转为 Image content block，
/// 其余类型的文档索引在 Phase 2（Document Memory）接入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    UserMessage {
        content: String,
        attachments: Vec<Attachment>,
    },
    /// 丢弃当前会话的短期上下文。宿主仅应在没有进行中查询时发送，持久化
    /// 快照与长期记忆由宿主在此事件之外按自己的存储边界处理。
    ClearConversation,
    SystemEvent {
        event_type: SystemEventType,
    },
}

#[derive(Debug)]
pub enum AgentState {
    Idle,
    Observing(AgentEvent),
    /// 模型流式 turn 进行中；`turn` 为已完成的模型轮数（0 起）。
    Querying {
        turn: u32,
    },
    /// 执行本轮 assistant 请求的 tool_use（多工具并发）。
    ExecutingTools {
        tool_uses: Vec<ToolUse>,
        turn: u32,
    },
    /// auto / manual / reactive 压缩（context/compact 于后续 Phase 落地）。
    Compacting {
        trigger: CompactTrigger,
    },
    Waiting,
    Completed,
    Failed(AgentError),
}

impl AgentState {
    pub fn kind(&self) -> StateKind {
        match self {
            Self::Idle => StateKind::Idle,
            Self::Observing(_) => StateKind::Observing,
            Self::Querying { .. } => StateKind::Querying,
            Self::ExecutingTools { .. } => StateKind::ExecutingTools,
            Self::Compacting { .. } => StateKind::Compacting,
            Self::Waiting => StateKind::Waiting,
            Self::Completed => StateKind::Completed,
            Self::Failed(_) => StateKind::Failed,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed(_))
    }
}

/// 无负载的状态判别，供 FSM 转换校验与测试断言使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateKind {
    Idle,
    Observing,
    Querying,
    ExecutingTools,
    Compacting,
    Waiting,
    Completed,
    Failed,
}
