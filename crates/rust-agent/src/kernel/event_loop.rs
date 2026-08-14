//! AgentKernel：FSM 驱动的流式工具调用循环（对齐 Harness
//! `engine/query_engine.py` + `query.py`）。
//!
//! Kernel 仅负责 Event Receive → State Transition → Service Dispatch，
//! 不直接操作 Memory / Skill；工具分发经 `ToolRuntime` 管线
//! （pre/post_tool_use hooks + 三态权限 + 输出预算，Phase 3）。

use std::collections::HashSet;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::FutureExt;
use futures::StreamExt;
use futures::channel::mpsc;
use serde_json::{Map, Value};

use crate::context::compact::{
    AutoCompactState, DEFAULT_PRESERVE_RECENT, auto_compact_if_needed, should_autocompact,
};
use crate::context::prompt_pipeline::permission_mode_section;
use crate::error::AgentError;
use crate::hooks::HookEvent;
use crate::kernel::context::ContextStore;
use crate::kernel::fsm;
use crate::kernel::messages::{
    ContentBlock, ConversationMessage, Role, ToolUse, sanitize_conversation_messages,
};
use crate::kernel::provider::AsyncSystemPromptProvider;
use crate::kernel::state::{AgentEvent, AgentState, CompactTrigger, SystemEventType};
use crate::kernel::stream_events::StreamEvent;
use crate::model_client::{
    DEFAULT_MAX_OUTPUT_TOKENS, ModelClient, ModelRequest, ModelStreamEvent, UsageSnapshot,
};
use crate::runtime_adapter::RuntimeAdapter;
use crate::tools::{Tool, ToolDef, ToolRuntime};

/// 事件总线容量（入站 AgentEvent）。
const EVENT_CHANNEL_CAPACITY: usize = 32;
/// Waiting 态回到 Idle 前的休眠间隔（对齐 AINS_PLAN 3.1 伪代码）。
const WAITING_SLEEP: Duration = Duration::from_millis(100);
/// 模型流没有产生事件时仍需周期性检查中断标志，否则网络层永久静默会
/// 让 Stop 只能等到底层请求超时。
const STREAM_INTERRUPT_POLL: Duration = Duration::from_millis(100);
/// Complete 后等待流关闭的收尾窗口：正常实现（网关）在 Complete 后立即
/// EOF，此窗口保留“流关闭瞬间的中断”竞态注入点（ExecutingTools 回填
/// 路径）；违反契约的实现（Complete 后保持连接）在窗口后放弃读取，
/// 避免 turn 永久挂起（review 修复：历史实现无限等待流关闭）。
const STREAM_COMPLETE_TAIL_TIMEOUT: Duration = Duration::from_millis(500);

/// Stable status payload emitted when the active query is cancelled by the
/// user.  UI consumers use this protocol value to discard partial streaming
/// output without conflating ordinary status updates (such as retries).
pub const QUERY_INTERRUPTED_STATUS: &str = "Query interrupted by user.";

#[derive(Clone)]
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
    /// 动态 system prompt provider（§12）：Querying 构造 ModelRequest 前
    /// await 一次，注入 memory recall 段。失败/无内容回落原提示。
    pub memory_provider: Option<Arc<dyn AsyncSystemPromptProvider>>,
}

impl std::fmt::Debug for AgentKernelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // memory_provider 是 trait object（无 Debug）；其余字段可展示。
        f.debug_struct("AgentKernelConfig")
            .field("cwd", &self.cwd)
            .field("max_turns", &self.max_turns)
            .field("idle_timeout", &self.idle_timeout)
            .field("system_prompt", &self.system_prompt)
            .field("model", &self.model)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("has_memory_provider", &self.memory_provider.is_some())
            .finish()
    }
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
            memory_provider: None,
        }
    }
}

/// Agent Kernel：仅持有状态、事件接收端、会话上下文与模型/工具入口。
pub struct AgentKernel<R: RuntimeAdapter> {
    state: AgentState,
    context: ContextStore,
    model: Arc<dyn ModelClient>,
    tools: ToolRuntime,
    events: mpsc::Receiver<AgentEvent>,
    stream: mpsc::UnboundedSender<StreamEvent>,
    config: AgentKernelConfig,
    /// 跨轮自动压缩状态（连续失败熔断计数等，Phase 5.5）。
    compact_state: AutoCompactState,
    /// 用户中断标志（Phase 7.1）：UI 经 [`Self::interrupt_handle`] 置位，Kernel 在
    /// 模型 turn / 工具批边界 check-and-clear，命中则中止本次查询回 Idle。
    /// 不经事件通道，避免与 Idle 的事件消费竞争。
    interrupt: Arc<AtomicBool>,
    _runtime: PhantomData<fn() -> R>,
}

impl<R: RuntimeAdapter> AgentKernel<R> {
    /// 构造 Kernel，返回（Kernel，事件发送端，UI 流事件接收端）。
    ///
    /// 裸工具列表默认装配 `default` 权限模式且不提供确认回调：只读工具可
    /// 执行，写工具 fail-closed。需要 UI 确认、hooks 或外置存储的宿主应使用
    /// [`Self::with_runtime`] 显式装配完整 `ToolRuntime`。
    pub fn new(
        model: Arc<dyn ModelClient>,
        tools: Vec<Box<dyn Tool>>,
        config: AgentKernelConfig,
    ) -> (
        Self,
        mpsc::Sender<AgentEvent>,
        mpsc::UnboundedReceiver<StreamEvent>,
    ) {
        let mut runtime = ToolRuntime::new();
        for tool in tools {
            runtime.register(tool);
        }
        Self::with_runtime(model, runtime, config)
    }

    /// 完整入口：宿主自行装配 ToolRuntime（权限引擎 / hooks / 外置存储）。
    pub fn with_runtime(
        model: Arc<dyn ModelClient>,
        tools: ToolRuntime,
        config: AgentKernelConfig,
    ) -> (
        Self,
        mpsc::Sender<AgentEvent>,
        mpsc::UnboundedReceiver<StreamEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (stream_tx, stream_rx) = mpsc::unbounded();
        (
            Self {
                state: AgentState::Idle,
                context: ContextStore::new(),
                model,
                tools,
                events: event_rx,
                stream: stream_tx,
                config,
                compact_state: AutoCompactState::default(),
                interrupt: Arc::new(AtomicBool::new(false)),
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

    /// 返回中断句柄：UI/宿主置位（`store(true, Ordering::Release)` 或更强）后，
    /// Kernel 在下一个模型 turn / 工具批边界中止本次查询回 Idle（协作式中断，
    /// 会话保活）。
    ///
    /// **Memory ordering 约定**：Kernel 通过 [`Ordering::SeqCst`] 读取并清除标志；
    /// 调用方在置位前如有其它共享写入（如设置取消原因等），必须以至少
    /// [`Ordering::Release`] 置位，保证写入对 Kernel 可见。仅置位布尔标志本身
    /// 时 [`Ordering::Relaxed`] 亦可工作（SeqCst reader 自带同步），但推荐统一
    /// 使用 Release 以避免混淆。
    pub fn interrupt_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.interrupt)
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
                            None => {
                                self.run_observational_hook(
                                    HookEvent::SessionEnd,
                                    lifecycle_payload(HookEvent::SessionEnd, &self.config.cwd),
                                )
                                .await;
                                AgentState::Completed
                            }
                        },
                        _ = sleep => AgentState::Waiting,
                    }
                }
                AgentState::Observing(AgentEvent::SystemEvent {
                    event_type: SystemEventType::Shutdown,
                }) => {
                    self.run_observational_hook(
                        HookEvent::SessionEnd,
                        lifecycle_payload(HookEvent::SessionEnd, &self.config.cwd),
                    )
                    .await;
                    AgentState::Completed
                }
                AgentState::Observing(AgentEvent::SystemEvent {
                    event_type: SystemEventType::Startup,
                }) => {
                    self.run_observational_hook(
                        HookEvent::SessionStart,
                        lifecycle_payload(HookEvent::SessionStart, &self.config.cwd),
                    )
                    .await;
                    AgentState::Idle
                }
                AgentState::Observing(AgentEvent::ClearConversation) => {
                    self.context.clear_conversation();
                    self.compact_state = AutoCompactState::default();
                    self.interrupt.store(false, Ordering::SeqCst);
                    AgentState::Idle
                }
                AgentState::Observing(event) => {
                    let payload = agent_event_hook_payload(&event, &self.config.cwd);
                    if self
                        .run_blocking_hook(HookEvent::UserPromptSubmit, payload)
                        .await
                    {
                        AgentState::Idle
                    } else {
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
                }
                AgentState::Querying { turn } => {
                    if self.interrupt_requested() {
                        // 模型 turn 前检测到中断：中止本次查询回 Idle（会话保活）。
                        self.emit(StreamEvent::Status {
                            message: QUERY_INTERRUPTED_STATUS.into(),
                        });
                        // 与自然完成对称：中断中止回合同样触发 Stop hook（观察性），
                        // 供宿主做轮次收尾（计费/日志/状态持久化）；review 修复：
                        // 历史三条中断路径均不执行 Stop hook，中断轮收尾缺失。
                        let mut payload = lifecycle_payload(HookEvent::Stop, &self.config.cwd);
                        payload.insert("stop_reason".into(), Value::String("interrupted".into()));
                        self.run_observational_hook(HookEvent::Stop, payload).await;
                        AgentState::Idle
                    } else if turn >= self.config.max_turns {
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
                        // 自动压缩：进入模型轮前检查上下文预算（对齐基线查询循环
                        // 起始的 auto_compact_if_needed），达阈值则先压缩再续轮。
                        if should_autocompact(
                            &self.context.conversation,
                            self.config.model.as_deref(),
                            &self.compact_state,
                        ) {
                            self.run_compaction(CompactTrigger::Auto, false).await;
                        }
                        let request = ModelRequest {
                            model: self.config.model.clone(),
                            messages: self.context.conversation.clone(),
                            system_prompt: self.build_system_prompt().await,
                            max_output_tokens: self.config.max_output_tokens,
                            tools: self.api_schemas(),
                        };
                        match self.stream_model_turn(request).await {
                            Some((message, usage)) => {
                                let tool_uses = message.tool_uses();
                                if let Err(reason) = validate_tool_use_ids(&tool_uses) {
                                    // ID 是 tool_use/tool_result 的唯一配对键；整批先校验
                                    // 再分发，避免半批已产生副作用后才发现协议违规。
                                    self.emit(StreamEvent::Error {
                                        message: format!(
                                            "Model returned an invalid tool_use batch: {reason}. \
                                             The turn was ignored to keep the session healthy."
                                        ),
                                        recoverable: true,
                                    });
                                    // review 修复：回合异常结束同样消费 Complete 尾窗
                                    // 内置位的陈旧中断标志，避免残留到下一次查询。
                                    let _ = self.interrupt_requested();
                                    AgentState::Idle
                                } else {
                                    self.context.conversation.push(message.clone());
                                    self.emit(StreamEvent::AssistantTurnComplete {
                                        message,
                                        usage,
                                        tool_metadata: self.context.tool_metadata.clone(),
                                    });
                                    if tool_uses.is_empty() {
                                        // 无工具请求，回答完成
                                        let mut payload =
                                            lifecycle_payload(HookEvent::Stop, &self.config.cwd);
                                        payload.insert(
                                            "stop_reason".into(),
                                            Value::String("tool_uses_empty".into()),
                                        );
                                        self.run_observational_hook(HookEvent::Stop, payload).await;
                                        // review 修复：消费 Complete 尾窗 / Stop hook 期间
                                        // 置位的陈旧中断标志——回合已自然完成（AssistantTurnComplete
                                        // 已发出），此中断应为 no-op；残留会使下一次查询在
                                        // Querying 入口被误中止。注意不得在 Idle 入口统一消费：
                                        // 新查询入队后、Querying 检查前置位的标志必须保留
                                        // （预置中断语义，见 interrupt_flag_* 回归测试）。
                                        let _ = self.interrupt_requested();
                                        AgentState::Idle
                                    } else {
                                        AgentState::ExecutingTools { tool_uses, turn }
                                    }
                                }
                            }
                            // 流错误 / 空 assistant：已上报事件，本轮忽略
                            None => AgentState::Idle,
                        }
                    }
                }
                AgentState::ExecutingTools { tool_uses, turn } => {
                    if self.interrupt_requested() {
                        // 工具批分发前检测到中断：不执行本批工具，但仍为每个
                        // tool_use 回填取消结果。这样内核保存的会话与 UI 镜像
                        // flush_pending_as_interrupted 的会话形状一致，恢复后不会
                        // 因 sanitize 丢弃一条曾在 UI 中显示为已中断的工具调用。
                        self.context.conversation.push(ConversationMessage {
                            role: Role::User,
                            content: tool_uses
                                .iter()
                                .map(|tool_use| ContentBlock::ToolResult {
                                    tool_use_id: tool_use.id.clone(),
                                    content: "interrupted".into(),
                                    is_error: true,
                                    result_metadata: Value::Null,
                                })
                                .collect(),
                        });
                        self.emit(StreamEvent::Status {
                            message: QUERY_INTERRUPTED_STATUS.into(),
                        });
                        // 中断回填后同样触发 Stop hook（观察性；见 Querying 中断路径）。
                        let mut payload = lifecycle_payload(HookEvent::Stop, &self.config.cwd);
                        payload.insert("stop_reason".into(), Value::String("interrupted".into()));
                        self.run_observational_hook(HookEvent::Stop, payload).await;
                        AgentState::Idle
                    } else {
                        // 注入查询级取消标志（review 接线）：UI 中断置位后，
                        // 沙箱后端（killpg / kill-on-close）终止运行中的进程树，
                        // 不再只能等 timeout。批结束后清除防陈旧残留。
                        // 注：工具批运行期间本循环不检查 interrupt（await 在
                        // dispatch_many 内），故共享标志不会被 check-and-clear
                        // 提前消费，沙箱轮询能看到置位。
                        self.tools
                            .set_query_cancel(Some(Arc::clone(&self.interrupt)));
                        for tool_use in &tool_uses {
                            self.emit(StreamEvent::ToolExecutionStarted {
                                tool_use_id: tool_use.id.clone(),
                                tool_name: tool_use.name.clone(),
                                tool_input: tool_use.input.clone(),
                            });
                        }
                        let outcomes = self
                            .tools
                            .dispatch_many(
                                &tool_uses,
                                &self.config.cwd,
                                &mut self.context.tool_metadata,
                            )
                            .await;
                        self.tools.set_query_cancel(None);
                        let mut results = Vec::with_capacity(tool_uses.len());
                        for (tool_use, outcome) in tool_uses.iter().zip(outcomes) {
                            self.emit(StreamEvent::ToolExecutionCompleted {
                                tool_use_id: tool_use.id.clone(),
                                tool_name: tool_use.name.clone(),
                                output: outcome.output.clone(),
                                is_error: outcome.is_error,
                                metadata: outcome.metadata.clone(),
                                // `dispatch_many` has returned, so the shared
                                // metadata contains this batch's mutations.
                                // The host persists after every completed tool
                                // result and must not retain the pre-dispatch
                                // AssistantTurnComplete snapshot.
                                tool_metadata: self.context.tool_metadata.clone(),
                            });
                            // 拒绝/失败不中止循环：作为 is_error 的 tool_result 回填
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: tool_use.id.clone(),
                                content: outcome.output,
                                is_error: outcome.is_error,
                                result_metadata: outcome.metadata,
                            });
                        }
                        // tool_result 以 user 消息回填，进入下一轮模型调用
                        self.context.conversation.push(ConversationMessage {
                            role: Role::User,
                            content: results,
                        });
                        AgentState::Querying {
                            turn: turn.saturating_add(1),
                        }
                    }
                }
                AgentState::Compacting { trigger } => {
                    // 手动/反应式压缩入口。压缩前检查中断标志：命中则跳过压缩
                    // 回 Idle 保活会话（压缩可推迟到下次查询前由 auto-compact 触发）。
                    if self.interrupt_requested() {
                        self.emit(StreamEvent::Status {
                            message: QUERY_INTERRUPTED_STATUS.into(),
                        });
                        // 中断中止回合同样触发 Stop hook（观察性；见 Querying 中断路径）。
                        let mut payload = lifecycle_payload(HookEvent::Stop, &self.config.cwd);
                        payload.insert("stop_reason".into(), Value::String("interrupted".into()));
                        self.run_observational_hook(HookEvent::Stop, payload).await;
                        AgentState::Idle
                    } else {
                        // 真实降级链与生命周期 hook 由 run_compaction 统一处理。
                        // 压缩成功后回 Querying 重置轮数预算续轮；PreCompact hook
                        // 阻断或未发生压缩时回 Idle 等待后续输入。
                        let force = matches!(trigger, CompactTrigger::Manual);
                        if self.run_compaction(trigger, force).await {
                            AgentState::Querying { turn: 0 }
                        } else {
                            self.emit(StreamEvent::Status {
                                message:
                                    "Compaction skipped (blocked by hook or nothing to compact)."
                                        .into(),
                            });
                            AgentState::Idle
                        }
                    }
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

    /// 协作式中断检查（check-and-clear）：返回中断标志当前值并复位。
    /// 在模型 turn / 工具批边界调用；不读事件通道，恰与 Idle 的事件消费解耦。
    /// 此外，回合自然结束的回 Idle 路径（无工具回答 / 校验错误）也会消费
    /// 残留标志（如 Complete 尾窗内置位），防止陈旧标志污染下一次查询。
    ///
    /// 使用 [`Ordering::SeqCst`] 保证：1) 能看到任意 ordering 的 caller 写入；
    /// 2) swap 的 store 侧与此前模型输出/工具结果写入构成 release 语义，
    /// 确保中断复位不对后续操作重排。
    fn interrupt_requested(&self) -> bool {
        self.interrupt.swap(false, Ordering::SeqCst)
    }

    async fn run_blocking_hook(&self, event: HookEvent, payload: Map<String, Value>) -> bool {
        let Some(hooks) = self.tools.hooks().cloned() else {
            return false;
        };
        let result = hooks.execute(event, &payload).await;
        if !result.blocked() {
            return false;
        }
        let reason = result.reason();
        self.emit(StreamEvent::Error {
            message: if reason.is_empty() {
                format!("{} hook blocked the event", event.as_str())
            } else {
                reason
            },
            recoverable: true,
        });
        true
    }

    async fn run_observational_hook(&self, event: HookEvent, payload: Map<String, Value>) {
        if let Some(hooks) = self.tools.hooks().cloned() {
            let _ = hooks.execute(event, &payload).await;
        }
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
        loop {
            // `stream.next().await` 本身可能永久 pending（例如网关连接静默），
            // 因此用短轮询把原子中断标志转化为可唤醒的取消点。
            let next_event = stream.next().fuse();
            let interrupt_tick = R::sleep(STREAM_INTERRUPT_POLL).fuse();
            futures::pin_mut!(next_event, interrupt_tick);
            let event = futures::select! {
                event = next_event => event,
                _ = interrupt_tick => {
                    if self.interrupt_requested() {
                        self.emit(StreamEvent::Status {
                            message: QUERY_INTERRUPTED_STATUS.into(),
                        });
                        return None;
                    }
                    continue;
                }
            };
            let Some(event) = event else { break };
            // 流内中断检查：UI 置位后中止本次 turn（不等待模型流自然结束），
            // 发 Status 通知并返回 None 回 Idle。
            if self.interrupt_requested() {
                self.emit(StreamEvent::Status {
                    message: QUERY_INTERRUPTED_STATUS.into(),
                });
                return None;
            }
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
                        message: format_retry_status(&message, attempt, max_attempts, delay_secs),
                    });
                }
                ModelStreamEvent::Complete { message, usage, .. } => {
                    finished = Some((message, usage));
                    // Complete 是协议终止事件，其后不应再有事件。正常流在
                    // 此立即 EOF：短窗收尾保留“流关闭瞬间的中断”竞态窗口
                    // （ExecutingTools 分发前回填 interrupted tool_result 的
                    // 注入点）；违反契约的实现（Complete 后保持连接）在窗口
                    // 后放弃读取，不无限挂起（review 修复：历史实现无限等
                    // 待流关闭，只能靠用户中断或底层请求超时逃逸）。
                    // Complete 后的事件不合法：tail 静默丢弃。
                    let tail = async { while stream.next().await.is_some() {} }.fuse();
                    let tail_timeout = R::sleep(STREAM_COMPLETE_TAIL_TIMEOUT).fuse();
                    futures::pin_mut!(tail, tail_timeout);
                    let tail_closed = futures::select! {
                        _ = tail => true,
                        _ = tail_timeout => false,
                    };
                    if !tail_closed {
                        // 流在窗口内未关闭：放弃等待，结束本 turn 的读取
                        // （下一次 select 会再次永久 pending）。
                        break;
                    }
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

    /// 执行上下文压缩（PreCompact hook → 四级降级链 → CompactProgress →
    /// PostCompact hook），返回是否发生压缩。auto 由 Querying 起始内联调用，
    /// manual/reactive 经 Compacting 状态进入。
    async fn run_compaction(&mut self, trigger: CompactTrigger, force: bool) -> bool {
        let mut payload = lifecycle_payload(HookEvent::PreCompact, &self.config.cwd);
        payload.insert(
            "trigger".into(),
            Value::String(format!("{trigger:?}").to_lowercase()),
        );
        if self.run_blocking_hook(HookEvent::PreCompact, payload).await {
            return false; // PreCompact hook 阻断压缩
        }

        // 降级链接受 messages 所有权并返回新列表。克隆 UI 流发送端供进度
        // 回调实时上报（而非批量后发），回调只捕获克隆端不借用 self
        // （self.model / &mut self.compact_state 已在同一调用中借用）。
        let messages = std::mem::take(&mut self.context.conversation);
        let progress_tx = self.stream.clone();
        let (compacted_messages, compacted) = auto_compact_if_needed(
            messages,
            self.model.as_ref(),
            self.config.model.as_deref(),
            &mut self.compact_state,
            trigger,
            DEFAULT_PRESERVE_RECENT,
            force,
            &mut |phase| {
                // UI 侧关闭接收端不影响压缩推进
                let _ = progress_tx.unbounded_send(StreamEvent::CompactProgress {
                    phase: phase.to_string(),
                    trigger,
                });
            },
        )
        .await;
        self.context.conversation = compacted_messages;

        // 压缩完成事件（§11.1）：仅实际发生压缩后 emit 一次，供宿主触发
        // ordered checkpoint + background durable extraction。
        if compacted {
            self.emit(StreamEvent::Compacted {
                trigger,
                // 在工具执行后的下一次 Querying 中压缩时，这里已经包含本轮
                // dispatch 对 metadata 的全部更新；宿主不可复用前一个
                // AssistantTurnComplete（它发生在 dispatch 之前）的快照。
                tool_metadata: self.context.tool_metadata.clone(),
            });
        }

        let mut payload = lifecycle_payload(HookEvent::PostCompact, &self.config.cwd);
        payload.insert(
            "trigger".into(),
            Value::String(format!("{trigger:?}").to_lowercase()),
        );
        payload.insert("success".into(), Value::Bool(compacted));
        self.run_observational_hook(HookEvent::PostCompact, payload)
            .await;
        compacted
    }

    fn api_schemas(&self) -> Vec<ToolDef> {
        self.tools.api_schemas()
    }

    /// 每轮生效的 system prompt：宿主基础提示（可选）+ 动态 memory 段
    /// （§12，provider 提供）+ 当前权限模式段。
    ///
    /// 拼装顺序固定：base → dynamic memory → permission mode（权限段必须
    /// 位于最后，Plan 下模型需要事先收到“勿调写工具”的指引，减少“试错→
    /// 被拒→再退出”的多余轮次；权限引擎仍是硬边界，本段仅为提前引导）。
    /// provider 失败/无内容返回 `None` 时回落 base + mode（§12.2）。
    async fn build_system_prompt(&self) -> Option<String> {
        let mode_section = permission_mode_section(self.tools.permissions().mode());
        let base = self.config.system_prompt.clone();
        let dynamic = match &self.config.memory_provider {
            Some(provider) => provider.provide(&self.context.conversation).await,
            None => None,
        };
        Some(match (base, dynamic) {
            (Some(base), Some(dynamic)) => {
                format!("{base}\n\n{dynamic}\n\n{mode_section}")
            }
            (Some(base), None) => format!("{base}\n\n{mode_section}"),
            (None, Some(dynamic)) => format!("{dynamic}\n\n{mode_section}"),
            (None, None) => mode_section,
        })
    }

    fn emit(&self, event: StreamEvent) {
        // UI 侧关闭接收端不影响循环推进
        let _ = self.stream.unbounded_send(event);
    }
}

/// 在任何工具分发前校验整批协议 ID，保证 tool_result 可唯一回配。
/// 单批数量同样受限：模型输出是协议信任面内的不可信输入，无上限批次
/// 会同时派生同数量 hook 进程 / 并发 execute 与事件洪泛（资源耗尽面）。
/// 超限与 ID 违规同口径：整批拒绝 + turn 忽略（review 修复）。
const MAX_TOOL_USE_BATCH: usize = 64;

fn validate_tool_use_ids(tool_uses: &[ToolUse]) -> Result<(), &'static str> {
    if tool_uses.len() > MAX_TOOL_USE_BATCH {
        return Err("tool_use batch exceeds the maximum size (64)");
    }
    let mut seen = HashSet::with_capacity(tool_uses.len());
    for tool_use in tool_uses {
        if tool_use.id.trim().is_empty() {
            return Err("tool_use IDs must not be empty or whitespace-only");
        }
        if !seen.insert(tool_use.id.as_str()) {
            return Err("tool_use IDs must be unique within an assistant turn");
        }
    }
    Ok(())
}

fn lifecycle_payload(event: HookEvent, cwd: &std::path::Path) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("event".into(), Value::String(event.as_str().into()));
    payload.insert("cwd".into(), Value::String(cwd.display().to_string()));
    payload
}

fn agent_event_hook_payload(event: &AgentEvent, cwd: &std::path::Path) -> Map<String, Value> {
    let mut payload = lifecycle_payload(HookEvent::UserPromptSubmit, cwd);
    if let AgentEvent::UserMessage {
        content,
        attachments,
    } = event
    {
        payload.insert("prompt".into(), Value::String(content.clone()));
        payload.insert(
            "attachment_count".into(),
            serde_json::json!(attachments.len()),
        );
    }
    payload
}

/// 渲染流内 Retry 事件的状态文案。ModelClient 协议约定：真实重试的
/// `attempt < max_attempts`；终态失败（不可重试/重试耗尽）以
/// `attempt == max_attempts` 承载，不得渲染为“重试中”误导 UI。
fn format_retry_status(message: &str, attempt: u32, max_attempts: u32, delay_secs: f32) -> String {
    if attempt >= max_attempts {
        format!("Model request failed: {message}")
    } else {
        format!(
            "Request failed; retrying in {delay_secs:.1}s \
             (attempt {attempt} of {max_attempts}): {message}"
        )
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn format_retry_status_renders_genuine_retry_with_delay() {
        let status = format_retry_status("boom", 1, 4, 2.5);
        assert!(status.contains("retrying in 2.5s"));
        assert!(status.contains("attempt 1 of 4"));
        assert!(status.contains("boom"));
    }

    #[test]
    fn format_retry_status_terminal_failure_not_rendered_as_retrying() {
        // 回归：终态事件（attempt == max_attempts，delay 0.0）曾被渲染为
        // "retrying in 0.0s (attempt 4 of 4)"，误导 UI 显示“重试中”
        let status = format_retry_status("request failed: [no_active_plan] x", 4, 4, 0.0);
        assert!(
            !status.contains("retrying"),
            "terminal failure must not say retrying: {status}"
        );
        assert!(status.contains("Model request failed"));
        assert!(status.contains("no_active_plan"));
    }
}
