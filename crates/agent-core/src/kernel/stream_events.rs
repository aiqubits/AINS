//! Kernel → UI 流事件（对齐 OpenHarness `engine/stream_events.py` 的七联合类型）。

use serde_json::Value;

use crate::kernel::messages::ConversationMessage;
use crate::kernel::state::CompactTrigger;
use crate::model_client::UsageSnapshot;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// 助手文本增量（基线 AssistantTextDelta）。
    AssistantTextDelta { text: String },
    /// 一个模型 turn 完成，携带完整 assistant 消息与用量（基线 AssistantTurnComplete）。
    AssistantTurnComplete {
        message: ConversationMessage,
        usage: UsageSnapshot,
    },
    /// 工具开始执行（基线 ToolExecutionStarted）。
    ToolExecutionStarted {
        tool_name: String,
        tool_input: Value,
    },
    /// 工具执行结束（基线 ToolExecutionCompleted；metadata 字段随 Phase 3 工具
    /// 输出预算引入）。
    ToolExecutionCompleted {
        tool_name: String,
        output: String,
        is_error: bool,
    },
    /// 错误上报（基线 ErrorEvent）；`recoverable = false` 表示会话已不可续。
    Error { message: String, recoverable: bool },
    /// 状态说明（基线 StatusEvent），如重试提示。
    Status { message: String },
    /// 压缩进度（基线 CompactProgressEvent 的最小子集；phase 取基线九阶段字面量）。
    CompactProgress {
        phase: String,
        trigger: CompactTrigger,
    },
}
