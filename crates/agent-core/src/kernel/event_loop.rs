//! AgentKernel：FSM 驱动的流式工具调用循环（对齐 OpenHarness
//! `engine/query_engine.py` + `query.py`）。
//!
//! Kernel 仅负责 Event Receive → State Transition → Service Dispatch，
//! 不直接操作 Memory / Skill；Phase 1 直接持有 ModelClient 与工具表，
//! RuntimeServices 聚合与三态权限 / hooks 在 Phase 2/3 接入后收敛。

use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::StreamExt;
use futures::channel::mpsc;

use crate::error::AgentError;
use crate::kernel::context::ContextStore;
use crate::kernel::fsm;
use crate::kernel::messages::{
    ContentBlock, ConversationMessage, Role, ToolUse, sanitize_conversation_messages,
};
use crate::kernel::state::{AgentEvent, AgentState, SystemEventType};
use crate::kernel::stream_events::StreamEvent;
use crate::model_client::{
    DEFAULT_MAX_OUTPUT_TOKENS, ModelClient, ModelRequest, ModelStreamEvent, UsageSnapshot,
};
use crate::runtime_adapter::RuntimeAdapter;
use crate::tools::{Tool, ToolContext, ToolDef, ToolResult};

/// 事件总线容量（入站 AgentEvent）。
const EVENT_CHANNEL_CAPACITY: usize = 32;
/// Waiting 态回到 Idle 前的休眠间隔（对齐 AINS_PLAN 3.1 伪代码）。
const WAITING_SLEEP: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct AgentKernelConfig {
    pub cwd: PathBuf,
    /// 单次用户输入内的模型轮数上限（对齐基线 QueryEngine 默认 `max_turns=8`）。
    pub max_turns: u32,
    /// Idle 态等待事件的超时；超时进入 Waiting。
    pub idle_timeout: Duration,
    /// 分段系统提示流水线（context/prompt_pipeline）落地前的整体系统提示。
    pub system_prompt: Option<String>,
    /// 目标模型；`None` 时由 AI Gateway 按套餐路由。
    pub model: Option<String>,
    pub max_output_tokens: u32,
}

impl Default for AgentKernelConfig {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            max_turns: 8,
            idle_timeout: Duration::from_secs(30),
            system_prompt: None,
            model: None,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}

/// Agent Kernel：仅持有状态、事件接收端、会话上下文与模型/工具入口。
pub struct AgentKernel<R: RuntimeAdapter> {
    state: AgentState,
    context: ContextStore,
    model: Arc<dyn ModelClient>,
    tools: HashMap<String, Box<dyn Tool>>,
    events: mpsc::Receiver<AgentEvent>,
    stream: mpsc::UnboundedSender<StreamEvent>,
    config: AgentKernelConfig,
    _runtime: PhantomData<fn() -> R>,
}

impl<R: RuntimeAdapter> AgentKernel<R> {
    /// 构造 Kernel，返回（Kernel，事件发送端，UI 流事件接收端）。
    pub fn new(
        model: Arc<dyn ModelClient>,
        tools: Vec<Box<dyn Tool>>,
        config: AgentKernelConfig,
    ) -> (
        Self,
        mpsc::Sender<AgentEvent>,
        mpsc::UnboundedReceiver<StreamEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (stream_tx, stream_rx) = mpsc::unbounded();
        let tools = tools
            .into_iter()
            .map(|tool| (tool.definition().name, tool))
            .collect();
        (
            Self {
                state: AgentState::Idle,
                context: ContextStore::new(),
                model,
                tools,
                events: event_rx,
                stream: stream_tx,
                config,
                _runtime: PhantomData,
            },
            event_tx,
            stream_rx,
        )
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    pub fn context(&self) -> &ContextStore {
        &self.context
    }

    /// 会话快照回载等场景直接操作上下文；回载后续跑请调 `prepare_continuation`。
    pub fn context_mut(&mut self) -> &mut ContextStore {
        &mut self.context
    }

    /// 会话末尾是否为待续轮：最后一条是含 `tool_result` 的 user 消息，
    /// 且最近的前置 assistant 消息含 `tool_use`（对齐基线 `has_pending_continuation`）。
    pub fn has_pending_continuation(&self) -> bool {
        let Some(last) = self.context.conversation.last() else {
            return false;
        };
        if last.role != Role::User
            || !last
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        {
            return false;
        }
        self.context
            .conversation
            .iter()
            .rev()
            .skip(1)
            .find(|message| message.role == Role::Assistant)
            .is_some_and(|message| !message.tool_uses().is_empty())
    }

    /// 中断恢复（对齐基线 `continue_pending`）：先 sanitize 历史；若仍为待续轮，
    /// 不追加新用户消息、直接置为 Querying 续跑。返回是否进入续跑状态。
    pub fn prepare_continuation(&mut self) -> bool {
        self.context.conversation =
            sanitize_conversation_messages(std::mem::take(&mut self.context.conversation));
        if self.has_pending_continuation() {
            // prepare_continuation 要求调用时处于 Idle 态（主循环外入口）。
            // debug_assert 守护此不变式，防止未来重构在不恰当的状态下调
            // 用此方法导致 FSM 被静默损坏。
            debug_assert!(
                matches!(self.state, AgentState::Idle),
                "prepare_continuation requires Idle state, got {:?}",
                self.state.kind()
            );
            self.state = AgentState::Querying { turn: 0 };
            true
        } else {
            false
        }
    }

    /// 事件循环主驱动。终态（Completed / Failed）保留在 `state` 中供调用方检查。
    pub async fn run(&mut self) -> Result<(), AgentError> {
        loop {
            let state = std::mem::replace(&mut self.state, AgentState::Idle);
            let from = state.kind();
            let next = match state {
                AgentState::Idle => {
                    let mut sleep = std::pin::pin!(R::sleep(self.config.idle_timeout).fuse());
                    futures::select! {
                        event = self.events.next() => match event {
                            Some(event) => AgentState::Observing(event),
                            // channel 关闭，优雅退出
                            None => AgentState::Completed,
                        },
                        _ = sleep => AgentState::Waiting,
                    }
                }
                AgentState::Observing(AgentEvent::SystemEvent {
                    event_type: SystemEventType::Shutdown,
                }) => AgentState::Completed,
                AgentState::Observing(AgentEvent::SystemEvent {
                    event_type: SystemEventType::Startup,
                }) => AgentState::Idle,
                AgentState::Observing(event) => {
                    // 历史先 sanitize 再追加新消息（对齐 submit_message 前置处理）
                    self.context.conversation = sanitize_conversation_messages(std::mem::take(
                        &mut self.context.conversation,
                    ));
                    match self.context.build(&event).await {
                        Ok(()) => {
                            if self.context.conversation.is_empty() {
                                // 空白输入未产生任何消息，无需查询模型
                                AgentState::Idle
                            } else {
                                AgentState::Querying { turn: 0 }
                            }
                        }
                        Err(err) => {
                            self.emit(StreamEvent::Error {
                                message: format!("Context build failed: {err}"),
                                recoverable: false,
                            });
                            AgentState::Failed(err)
                        }
                    }
                }
                AgentState::Querying { turn } => {
                    if turn >= self.config.max_turns {
                        self.emit(StreamEvent::Error {
                            message: format!(
                                "Exceeded maximum turn limit ({})",
                                self.config.max_turns
                            ),
                            // 对齐基线：MaxTurnsExceeded 仅中止本次 query，
                            // 回 Idle 后会话可继续接受新输入（Failed 保留给
                            // 真正不可恢复的内核故障）。
                            recoverable: true,
                        });
                        AgentState::Idle
                    } else {
                        let request = ModelRequest {
                            model: self.config.model.clone(),
                            messages: self.context.conversation.clone(),
                            system_prompt: self.config.system_prompt.clone(),
                            max_output_tokens: self.config.max_output_tokens,
                            tools: self.api_schemas(),
                        };
                        match self.stream_model_turn(request).await {
                            Some((message, usage)) => {
                                let tool_uses = message.tool_uses();
                                self.context.conversation.push(message.clone());
                                self.emit(StreamEvent::AssistantTurnComplete { message, usage });
                                if tool_uses.is_empty() {
                                    // 无工具请求，回答完成
                                    AgentState::Idle
                                } else {
                                    AgentState::ExecutingTools { tool_uses, turn }
                                }
                            }
                            // 流错误 / 空 assistant：已上报事件，本轮忽略
                            None => AgentState::Idle,
                        }
                    }
                }
                AgentState::ExecutingTools { tool_uses, turn } => {
                    let mut results = Vec::with_capacity(tool_uses.len());
                    for tool_use in &tool_uses {
                        self.emit(StreamEvent::ToolExecutionStarted {
                            tool_name: tool_use.name.clone(),
                            tool_input: tool_use.input.clone(),
                        });
                        let outcome = self.dispatch_tool(tool_use).await;
                        self.emit(StreamEvent::ToolExecutionCompleted {
                            tool_name: tool_use.name.clone(),
                            output: outcome.output.clone(),
                            is_error: outcome.is_error,
                        });
                        // 拒绝/失败不中止循环：作为 is_error 的 tool_result 回填
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: tool_use.id.clone(),
                            content: outcome.output,
                            is_error: outcome.is_error,
                        });
                    }
                    // tool_result 以 user 消息回填，进入下一轮模型调用
                    self.context.conversation.push(ConversationMessage {
                        role: Role::User,
                        content: results,
                    });
                    AgentState::Querying { turn: turn + 1 }
                }
                AgentState::Compacting { trigger } => {
                    // context/compact 于后续 Phase 落地；当前占位直通
                    self.emit(StreamEvent::CompactProgress {
                        phase: "compact_failed".into(),
                        trigger,
                    });
                    AgentState::Idle
                }
                AgentState::Waiting => {
                    R::sleep(WAITING_SLEEP).await;
                    AgentState::Idle
                }
                terminal @ (AgentState::Completed | AgentState::Failed(_)) => {
                    self.state = terminal;
                    break;
                }
            };
            debug_assert!(
                fsm::is_valid_transition(from, next.kind()),
                "invalid FSM transition: {from:?} -> {:?}",
                next.kind()
            );
            self.state = next;
        }
        Ok(())
    }

    /// 流式消费一个模型 turn：delta / retry 转 StreamEvent 推 UI，Complete 返回
    /// assistant 消息。错误与空 assistant 均已上报事件并返回 `None`（本轮忽略，
    /// 会话保持存活——偏差记录见对齐清单：基线对协议违规直接抛异常）。
    async fn stream_model_turn(
        &self,
        request: ModelRequest,
    ) -> Option<(ConversationMessage, UsageSnapshot)> {
        let mut stream = match self.model.stream_response(request).await {
            Ok(stream) => stream,
            Err(error) => {
                self.emit(StreamEvent::Error {
                    message: transport_error_message(&error),
                    recoverable: true,
                });
                return None;
            }
        };
        let mut finished = None;
        while let Some(event) = stream.next().await {
            match event {
                ModelStreamEvent::TextDelta { text } => {
                    self.emit(StreamEvent::AssistantTextDelta { text });
                }
                ModelStreamEvent::Retry {
                    message,
                    attempt,
                    max_attempts,
                    delay_secs,
                } => {
                    self.emit(StreamEvent::Status {
                        message: format!(
                            "Request failed; retrying in {delay_secs:.1}s \
                             (attempt {attempt} of {max_attempts}): {message}"
                        ),
                    });
                }
                ModelStreamEvent::Complete { message, usage, .. } => {
                    finished = Some((message, usage));
                }
            }
        }
        let (mut message, usage) = match finished {
            Some(finished) => finished,
            None => {
                self.emit(StreamEvent::Error {
                    message: "Model stream finished without a final message".into(),
                    recoverable: true,
                });
                return None;
            }
        };
        // 强制 role=Assistant：模型可能返回错误的 role，
        // 若为 User 会破坏会话交替结构且 sanitize 不会识别其 tool_use 为 pending。
        message.role = Role::Assistant;
        if message.is_effectively_empty() {
            self.emit(StreamEvent::Error {
                message: "Model returned an empty assistant message. \
                          The turn was ignored to keep the session healthy."
                    .into(),
                recoverable: true,
            });
            return None;
        }
        Some((message, usage))
    }

    /// 单个 tool_use 的分发：未知工具与执行异常均归一化为 is_error 的
    /// ToolResult（对齐基线合成 error tool_result 语义）；pre/post_tool_use
    /// hooks 与三态权限在 Phase 3 插入本路径。
    async fn dispatch_tool(&mut self, tool_use: &ToolUse) -> ToolResult {
        match self.tools.get(&tool_use.name) {
            None => ToolResult::err(format!("Unknown tool: {}", tool_use.name)),
            Some(tool) => {
                let mut ctx = ToolContext {
                    cwd: &self.config.cwd,
                    metadata: &mut self.context.tool_metadata,
                };
                match tool.execute(tool_use.input.clone(), &mut ctx).await {
                    Ok(result) => result,
                    Err(error) => {
                        ToolResult::err(format!("Tool {} failed: {error}", tool_use.name))
                    }
                }
            }
        }
    }

    fn api_schemas(&self) -> Vec<ToolDef> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    fn emit(&self, event: StreamEvent) {
        // UI 侧关闭接收端不影响循环推进
        let _ = self.stream.unbounded_send(event);
    }
}

/// 传输层错误的用户可读归一化（对齐基线网络启发式分类）。
///
/// TODO: 替换为结构化错误变体（`AgentError::Model { is_transport: bool }`），
///       消除脆弱的字符串启发式分类。
fn transport_error_message(error: &AgentError) -> String {
    let text = error.to_string();
    let lowered = text.to_lowercase();
    if lowered.contains("connect") || lowered.contains("timeout") || lowered.contains("network") {
        format!("Network error: {text}. Check your internet connection and try again.")
    } else {
        format!("API error: {text}")
    }
}
