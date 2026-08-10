//! Kernel → UI 流事件（对齐 OpenHarness `engine/stream_events.py` 的七联合类型）。

use serde_json::Value;

use crate::kernel::messages::ConversationMessage;
use crate::kernel::state::CompactTrigger;
use crate::model_client::UsageSnapshot;
use crate::tools::ToolMetadata;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// 助手文本增量（基线 AssistantTextDelta）。
    AssistantTextDelta { text: String },
    /// 一个模型 turn 完成，携带完整 assistant 消息、用量与 turn 级
    /// tool metadata（基线 AssistantTurnComplete）。
    AssistantTurnComplete {
        message: ConversationMessage,
        usage: UsageSnapshot,
        /// P2（§10.2）：turn 级结构化 tool metadata；checkpoint 的
        /// current_state / active_artifacts 等字段由此映射。
        tool_metadata: ToolMetadata,
    },
    /// 工具开始执行（基线 ToolExecutionStarted）。
    ToolExecutionStarted {
        /// `tool_use` 协议 id：UI/镜像据此与 Completed 精确配对，
        /// 同名工具（未来并行执行时）不依赖名称 FIFO 启发。
        tool_use_id: String,
        tool_name: String,
        tool_input: Value,
    },
    /// 工具执行结束（对齐基线 ToolExecutionCompleted）。
    ToolExecutionCompleted {
        /// 与 Started 同源的 `tool_use` 协议 id（唯一配对键）。
        tool_use_id: String,
        tool_name: String,
        output: String,
        is_error: bool,
        metadata: Value,
        /// Tool dispatch has completed before this event is emitted, so this
        /// is the current session-wide metadata. Hosts persist it alongside
        /// the tool-result conversation snapshot for crash-safe recovery.
        tool_metadata: ToolMetadata,
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
    /// 压缩**完成**事件（§11.1）：仅 `run_compaction` 实际发生压缩
    /// （`was_compacted == true`）后 emit 一次。宿主以它为 checkpoint /
    /// extraction 的触发器，但必须使用自己保留的未折叠 ConversationMirror
    /// 快照；不能把 Kernel 的压缩工作上下文覆盖到会话持久化中。
    /// `tool_metadata` 是压缩发生时的当前状态，不能复用工具调用前的
    /// `AssistantTurnComplete` 快照。`CompactProgress` 是多次进度事件，
    /// 不可作为生产写入触发。
    Compacted {
        trigger: CompactTrigger,
        tool_metadata: ToolMetadata,
    },
}
