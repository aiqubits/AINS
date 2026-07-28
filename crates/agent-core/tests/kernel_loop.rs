//! Phase 1 工具循环单测 + 完整 Agent Loop 集成测试（Native）。
//!
//! 用例对照 OpenHarness `tests/test_engine/test_query_engine.py`：
//! plain text / 工具循环 / 工具异常合成 error result / 未知工具 / max_turns /
//! retry 状态上报 / 空 assistant 丢弃 / sanitize 悬空 tool_use / continue_pending。

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use agent_core::TokioRuntimeAdapter;
use agent_core::error::ToolError;
use agent_core::kernel::{
    AgentEvent, AgentKernel, AgentKernelConfig, AgentState, ContentBlock, ConversationMessage,
    Role, ScriptedModelClient, StreamEvent, SystemEventType,
};
use agent_core::model_client::{ModelStreamEvent, UsageSnapshot};
use agent_core::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "echo".into(),
            description: "echo back the input text".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let text = input["text"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing `text`".into()))?;
        ctx.metadata.append_work_log(format!("echo: {text}"));
        Ok(ToolResult::ok(format!("echo: {text}")))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

struct FailingTool;

#[async_trait::async_trait]
impl Tool for FailingTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "boom".into(),
            description: "always fails".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn execute(
        &self,
        _input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::Execution("kaboom".into()))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

fn usage() -> UsageSnapshot {
    UsageSnapshot {
        input_tokens: 1,
        output_tokens: 1,
    }
}

fn test_config() -> AgentKernelConfig {
    AgentKernelConfig {
        idle_timeout: Duration::from_secs(5),
        ..AgentKernelConfig::default()
    }
}

/// 运行 kernel 直到事件通道关闭，返回（kernel，全部 UI 流事件）。
async fn run_kernel(
    model: Arc<ScriptedModelClient>,
    tools: Vec<Box<dyn Tool>>,
    config: AgentKernelConfig,
    events: Vec<AgentEvent>,
) -> (AgentKernel<TokioRuntimeAdapter>, Vec<StreamEvent>) {
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::new(model, tools, config);
    for event in events {
        event_tx.try_send(event).unwrap();
    }
    drop(event_tx);
    kernel.run().await.unwrap();
    let mut collected = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        collected.push(event);
    }
    (kernel, collected)
}

fn user_message(text: &str) -> AgentEvent {
    AgentEvent::UserMessage {
        content: text.into(),
        attachments: vec![],
    }
}

#[tokio::test]
async fn plain_text_reply_streams_delta_then_turn_complete() {
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("你好！", usage()),
    ]));
    let (kernel, events) = run_kernel(
        Arc::clone(&model),
        vec![Box::new(EchoTool)],
        test_config(),
        vec![user_message("hi")],
    )
    .await;

    assert!(matches!(
        events[0],
        StreamEvent::AssistantTextDelta { ref text } if text == "你好！"
    ));
    assert!(matches!(
        events[1],
        StreamEvent::AssistantTurnComplete { .. }
    ));
    // user + assistant
    assert_eq!(kernel.context().conversation.len(), 2);
    assert!(matches!(kernel.state(), AgentState::Completed));

    // 请求携带 system prompt 缺省（None）、工具 schema 与消息
    let requests = model.recorded_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].name, "echo");
    assert_eq!(requests[0].messages.len(), 1);
}

#[tokio::test]
async fn tool_loop_executes_and_backfills_tool_result() {
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                Some("让我调用工具"),
                "toolu_1",
                "echo",
                json!({"text": "ping"}),
            ),
            usage(),
        ),
        ScriptedModelClient::text_turn("完成：pong", usage()),
    ]));
    let (kernel, events) = run_kernel(
        Arc::clone(&model),
        vec![Box::new(EchoTool)],
        test_config(),
        vec![user_message("run echo")],
    )
    .await;

    let started: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolExecutionStarted { .. }))
        .collect();
    let completed: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolExecutionCompleted { .. }))
        .collect();
    assert_eq!(started.len(), 1);
    assert_eq!(completed.len(), 1);
    assert!(matches!(
        completed[0],
        StreamEvent::ToolExecutionCompleted { output, is_error, .. }
            if output == "echo: ping" && !is_error
    ));

    // user / assistant+tool_use / user+tool_result / assistant（对齐基线 len==4）
    let conversation = &kernel.context().conversation;
    assert_eq!(conversation.len(), 4);
    assert_eq!(conversation[2].role, Role::User);
    assert!(matches!(
        &conversation[2].content[0],
        ContentBlock::ToolResult { tool_use_id, content, is_error }
            if tool_use_id == "toolu_1" && content == "echo: ping" && !is_error
    ));
    // 第二次模型请求携带了回填的 tool_result
    let requests = model.recorded_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages.len(), 3);
    // 工具经 ctx 写入了跨轮状态袋
    assert_eq!(kernel.context().tool_metadata.work_log, vec!["echo: ping"]);
}

#[tokio::test]
async fn failing_tool_synthesizes_error_result_and_loop_recovers() {
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(None, "toolu_1", "boom", json!({})),
            usage(),
        ),
        ScriptedModelClient::text_turn("已从工具失败中恢复", usage()),
    ]));
    let (kernel, events) = run_kernel(
        model,
        vec![Box::new(FailingTool)],
        test_config(),
        vec![user_message("trigger failure")],
    )
    .await;

    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::ToolExecutionCompleted { output, is_error: true, .. }
            if output.starts_with("Tool boom failed:")
    )));
    // 每个 tool_use 都有配对 tool_result（is_error 回填，循环继续）
    let conversation = &kernel.context().conversation;
    assert_eq!(conversation.len(), 4);
    assert!(matches!(
        &conversation[2].content[0],
        ContentBlock::ToolResult { is_error: true, .. }
    ));
    assert!(matches!(kernel.state(), AgentState::Completed));
}

#[tokio::test]
async fn unknown_tool_returns_error_result() {
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(None, "toolu_1", "ghost", json!({})),
            usage(),
        ),
        ScriptedModelClient::text_turn("好的", usage()),
    ]));
    let (kernel, events) = run_kernel(
        model,
        vec![Box::new(EchoTool)],
        test_config(),
        vec![user_message("call ghost")],
    )
    .await;

    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::ToolExecutionCompleted { output, is_error: true, .. }
            if output == "Unknown tool: ghost"
    )));
    // 验证 conversation 包含正确的 tool_result（tool_use_id 配对）
    let conversation = &kernel.context().conversation;
    assert_eq!(conversation.len(), 4);
    assert_eq!(conversation[0].role, Role::User);
    assert_eq!(conversation[1].role, Role::Assistant);
    assert_eq!(conversation[2].role, Role::User);
    assert!(matches!(
        &conversation[2].content[0],
        ContentBlock::ToolResult { tool_use_id, content, is_error: true }
            if tool_use_id == "toolu_1" && content == "Unknown tool: ghost"
    ));
}

#[tokio::test]
async fn max_turns_exceeded_returns_to_idle_and_kernel_completes() {
    // 模型每轮都请求工具：max_turns=2 → 第三次查询前回到 Idle
    let tool_turn = || {
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                None,
                "toolu_loop",
                "echo",
                json!({"text": "again"}),
            ),
            usage(),
        )
    };
    let model = Arc::new(ScriptedModelClient::new(vec![tool_turn(), tool_turn()]));
    let config = AgentKernelConfig {
        max_turns: 2,
        ..test_config()
    };
    let (kernel, events) = run_kernel(
        model,
        vec![Box::new(EchoTool)],
        config,
        vec![user_message("loop forever")],
    )
    .await;

    // P1 修复：MaxTurnsExceeded 不再杀死 Kernel，回 Idle 后 channel 关闭 → Completed
    assert!(matches!(kernel.state(), AgentState::Completed));
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Error { message, recoverable: true }
            if message == "Exceeded maximum turn limit (2)"
    )));
}

#[tokio::test]
async fn retry_event_surfaces_as_status() {
    let mut script = vec![ModelStreamEvent::Retry {
        message: "rate limited".into(),
        attempt: 1,
        max_attempts: 4,
        delay_secs: 1.5,
    }];
    script.extend(ScriptedModelClient::text_turn("恢复正常", usage()));
    let model = Arc::new(ScriptedModelClient::new(vec![script]));
    let (_, events) = run_kernel(model, vec![], test_config(), vec![user_message("hi")]).await;

    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Status { message }
            if message.contains("retrying in 1.5s") && message.contains("attempt 1 of 4")
    )));
}

#[tokio::test]
async fn empty_assistant_message_is_ignored_with_error_event() {
    let model = Arc::new(ScriptedModelClient::new(vec![vec![
        ModelStreamEvent::Complete {
            message: ConversationMessage {
                role: Role::Assistant,
                content: vec![],
            },
            usage: usage(),
            stop_reason: None,
        },
    ]]));
    let (kernel, events) = run_kernel(model, vec![], test_config(), vec![user_message("hi")]).await;

    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Error { message, recoverable: true }
            if message.contains("empty assistant message")
    )));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StreamEvent::AssistantTurnComplete { .. }))
    );
    // 仅剩用户消息，空 turn 未入会话
    assert_eq!(kernel.context().conversation.len(), 1);
}

#[tokio::test]
async fn model_transport_error_is_reported_and_session_survives() {
    // 脚本耗尽 → stream_response 返回 Err → Error 事件，会话保持存活
    let model = Arc::new(ScriptedModelClient::new(vec![]));
    let (kernel, events) = run_kernel(model, vec![], test_config(), vec![user_message("hi")]).await;

    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Error { message, recoverable: true } if message.starts_with("API error:")
    )));
    assert!(matches!(kernel.state(), AgentState::Completed));
}

#[tokio::test]
async fn new_prompt_sanitizes_dangling_tool_use() {
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("干净的历史", usage()),
    ]));
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![],
        test_config(),
    );
    // 回载带悬空 tool_use 的历史（中断残留）
    kernel.context_mut().conversation = vec![
        ConversationMessage::from_user_text("旧问题"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_missing_output".into(),
                name: "echo".into(),
                input: json!({}),
            }],
        },
    ];
    event_tx.try_send(user_message("新问题")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();
    while stream_rx.try_recv().is_ok() {}

    let conversation = &kernel.context().conversation;
    // 悬空 tool_use 已被修剪：旧user / 新user / assistant
    assert_eq!(conversation.len(), 3);
    assert!(!conversation.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "call_missing_output"))
    }));
}

#[tokio::test]
async fn continue_pending_resumes_tool_loop_without_new_user_message() {
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("基于工具结果的最终回答", usage()),
    ]));
    let (mut kernel, event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![],
        test_config(),
    );
    // 回载中断会话：末尾是已回填的 tool_result 待续轮
    kernel.context_mut().conversation = vec![
        ConversationMessage::from_user_text("查一下"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "echo".into(),
                input: json!({"text": "x"}),
            }],
        },
        ConversationMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "echo: x".into(),
                is_error: false,
            }],
        },
    ];
    assert!(kernel.has_pending_continuation());
    assert!(kernel.prepare_continuation());
    drop(event_tx);
    kernel.run().await.unwrap();

    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::AssistantTurnComplete { .. }))
    );
    // 未追加新用户消息：3 条回载 + 1 条 assistant
    assert_eq!(kernel.context().conversation.len(), 4);
    // 请求消息即回载的 3 条
    assert_eq!(model.recorded_requests()[0].messages.len(), 3);
}

#[test]
fn has_pending_continuation_returns_false_without_preceding_assistant() {
    // 末尾 user 消息含 tool_result，但前一条不是 assistant（无 tool_use 配对）
    let mut kernel = {
        let (k, _, _) = AgentKernel::<TokioRuntimeAdapter>::new(
            Arc::new(ScriptedModelClient::new(vec![])),
            vec![],
            test_config(),
        );
        k
    };
    kernel.context_mut().conversation = vec![
        ConversationMessage::from_user_text("旧问题"),
        ConversationMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "orphan".into(),
                content: "无配对 result".into(),
                is_error: false,
            }],
        },
    ];
    assert!(
        !kernel.has_pending_continuation(),
        "orphan ToolResult without preceding Assistant tool_use is not a pending continuation"
    );
    assert!(
        !kernel.prepare_continuation(),
        "prepare_continuation should return false for non-pending conversation"
    );
}

#[test]
fn prepare_continuation_returns_false_for_completed_conversation() {
    // 会话末尾是纯文本 assistant 回答——无待续工具调用
    let mut kernel = {
        let (k, _, _) = AgentKernel::<TokioRuntimeAdapter>::new(
            Arc::new(ScriptedModelClient::new(vec![])),
            vec![],
            test_config(),
        );
        k
    };
    kernel.context_mut().conversation = vec![
        ConversationMessage::from_user_text("hi"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "好的".into(),
            }],
        },
    ];
    assert!(!kernel.has_pending_continuation());
    assert!(!kernel.prepare_continuation());
}

#[tokio::test]
async fn shutdown_event_completes_kernel_without_query() {
    let model = Arc::new(ScriptedModelClient::new(vec![]));
    let (kernel, events) = run_kernel(
        Arc::clone(&model),
        vec![],
        test_config(),
        vec![AgentEvent::SystemEvent {
            event_type: SystemEventType::Shutdown,
        }],
    )
    .await;

    assert!(matches!(kernel.state(), AgentState::Completed));
    assert!(events.is_empty());
    assert!(model.recorded_requests().is_empty());
}

#[tokio::test]
async fn startup_event_transitions_to_idle() {
    // Startup → Idle 是一个无副作用的 no-op 转换，不产生 UI 事件、
    // 不修改上下文。此测试守护该转换在未来不被意外破坏。
    let (kernel, events) = run_kernel(
        Arc::new(ScriptedModelClient::new(vec![])),
        vec![],
        test_config(),
        vec![AgentEvent::SystemEvent {
            event_type: SystemEventType::Startup,
        }],
    )
    .await;

    // Startup 后 Idle 会因 idle_timeout → Waiting → Idle → channel 关闭 → Completed
    assert!(matches!(kernel.state(), AgentState::Completed));
    // 无任何流事件（Startup → Idle 不下发任何事件）
    assert!(events.is_empty());
}

#[tokio::test]
async fn multi_tool_use_executed_sequentially_and_aggregated_into_single_message() {
    // 单轮 assistant 消息携带两个 tool_use → 顺序执行 → 结果聚合成一条
    // user 消息（event_loop.rs:229-254），这是与基线并行 gather 的偏差点。
    let assistant = ConversationMessage {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "需要两个工具".into(),
            },
            ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "echo".into(),
                input: json!({"text": "hello"}),
            },
            ContentBlock::ToolUse {
                id: "toolu_2".into(),
                name: "echo".into(),
                input: json!({"text": "world"}),
            },
        ],
    };
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(assistant.clone(), usage()),
        ScriptedModelClient::text_turn("两个工具都跑完了", usage()),
    ]));
    let (kernel, events) = run_kernel(
        model,
        vec![Box::new(EchoTool)],
        test_config(),
        vec![user_message("run both")],
    )
    .await;

    // 两个 ToolExecutionStarted + 两个 ToolExecutionCompleted
    let started: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolExecutionStarted { .. }))
        .collect();
    let completed: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolExecutionCompleted { .. }))
        .collect();
    assert_eq!(started.len(), 2);
    assert_eq!(completed.len(), 2);
    assert_eq!(
        started[0],
        &StreamEvent::ToolExecutionStarted {
            tool_name: "echo".into(),
            tool_input: json!({"text": "hello"}),
        }
    );
    assert_eq!(
        started[1],
        &StreamEvent::ToolExecutionStarted {
            tool_name: "echo".into(),
            tool_input: json!({"text": "world"}),
        }
    );

    // 会话结构：user / assistant(含 tool_use) / user(聚合两个 tool_result) / assistant
    let conversation = &kernel.context().conversation;
    assert_eq!(conversation.len(), 4);
    // 第 3 条（index 2）是单条 user 消息，包含两个 tool_result
    let results_msg = &conversation[2];
    assert_eq!(results_msg.role, Role::User);
    assert_eq!(results_msg.content.len(), 2);
    assert!(matches!(&results_msg.content[0], ContentBlock::ToolResult {
        tool_use_id, content, is_error: false
    } if tool_use_id == "toolu_1" && content == "echo: hello"));
    assert!(matches!(&results_msg.content[1], ContentBlock::ToolResult {
        tool_use_id, content, is_error: false
    } if tool_use_id == "toolu_2" && content == "echo: world"));
}

#[tokio::test]
async fn complete_message_with_wrong_role_is_forced_to_assistant() {
    // 模型返回 role=User 的 Complete 消息 → kernel 强制矫正为 Assistant
    let bad_message = ConversationMessage {
        role: Role::User, // 错误：应为 Assistant
        content: vec![ContentBlock::Text {
            text: "模型返回了错误 role".into(),
        }],
    };
    let model = Arc::new(ScriptedModelClient::new(vec![vec![
        ModelStreamEvent::Complete {
            message: bad_message,
            usage: usage(),
            stop_reason: None,
        },
    ]]));
    let (kernel, events) = run_kernel(model, vec![], test_config(), vec![user_message("hi")]).await;

    // 仍然收到了正常的 turn complete
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::AssistantTurnComplete { .. }))
    );
    // 会话中追加的 assistant 消息 role 已矫正
    let conversation = &kernel.context().conversation;
    assert_eq!(conversation.len(), 2);
    assert_eq!(conversation[1].role, Role::Assistant);
    assert_eq!(conversation[1].text(), "模型返回了错误 role");
}

#[tokio::test]
async fn max_turns_exceeded_session_survives_for_subsequent_input() {
    // P1 验证：MaxTurnsExceeded 不杀死 Kernel，会话可继续接受新输入。
    // 通过 spawn 旁路运行 kernel，让主线程分步投递事件并观测流。
    use std::time::Duration;

    let tool_turn = || {
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                None,
                "toolu_loop",
                "echo",
                json!({"text": "again"}),
            ),
            usage(),
        )
    };
    // 两轮 tool 脚本 + 一轮纯文本回复
    let model = Arc::new(ScriptedModelClient::new(vec![
        tool_turn(),
        tool_turn(),
        ScriptedModelClient::text_turn("第二轮回答", usage()),
    ]));
    let config = AgentKernelConfig {
        max_turns: 2,
        ..test_config()
    };
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![Box::new(EchoTool)],
        config,
    );

    // 第一轮消息：触发 tool loop 直到 max_turns=2 耗尽
    event_tx.try_send(user_message("loop")).unwrap();

    // spawn kernel，让它跑到 Idle（max_turns 耗尽）后继续等待新事件
    let handle = tokio::spawn(async move { kernel.run().await });

    // 等待 MaxTurns 超限的 Error 事件出现（证明已回到 Idle）
    let mut saw_max_turns_error = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match stream_rx.try_recv() {
            Ok(StreamEvent::Error { message, .. }) if message.contains("Exceeded maximum turn") => {
                saw_max_turns_error = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    assert!(saw_max_turns_error, "should have seen max turns error");

    // 第二轮消息：验证 kernel 仍在运行且能处理新输入
    event_tx.try_send(user_message("second round")).unwrap();
    drop(event_tx);

    // 收集剩余的流事件
    let mut second_response = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match stream_rx.try_recv() {
            Ok(StreamEvent::AssistantTurnComplete { message, .. }) => {
                if message.text() == "第二轮回答" {
                    second_response = true;
                    break;
                }
            }
            Ok(_) => continue,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    assert!(
        second_response,
        "kernel should process second message after max turns"
    );

    // kernel 正常结束
    let result = handle.await.unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn continuation_resets_turn_counter_after_prepare_continuation() {
    // P1 验证：prepare_continuation 将 turn 重置为 0，允许新会话
    // 使用完整的 max_turns 轮次（非累计历史消耗）。
    let tool_turn = || {
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                None,
                "toolu_loop",
                "echo",
                json!({"text": "again"}),
            ),
            usage(),
        )
    };
    let model = Arc::new(ScriptedModelClient::new(vec![
        tool_turn(), // 第 1 轮
        tool_turn(), // 第 2 轮 → max_turns=2 触发 Idle
    ]));
    let config = AgentKernelConfig {
        max_turns: 2,
        idle_timeout: Duration::from_secs(1),
        ..test_config()
    };
    let (mut kernel, event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::new(model, vec![Box::new(EchoTool)], config);

    // 填入待续轮会话（已消耗 0 轮，末尾是待续 tool_result）
    kernel.context_mut().conversation = vec![
        ConversationMessage::from_user_text("do it"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "echo".into(),
                input: json!({}),
            }],
        },
        ConversationMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "echo: ok".into(),
                is_error: false,
            }],
        },
    ];
    assert!(kernel.prepare_continuation());
    drop(event_tx);
    kernel.run().await.unwrap();

    // 即使历史已有 0 轮消费，prepare_continuation 重置 turn=0，
    // 允许再跑 max_turns=2 轮（共计 2 次模型请求）
    // 第 1 轮：tool_use→执行→tool_result→第 2 轮；
    // 第 2 轮：tool_use→执行→tool_result→转 Querying 时 turn=2 ≥ max_turns → Idle
    let mut collect = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        collect.push(event);
    }
    // 应该有 2 次 AssistantTurnComplete（对应 2 次模型调用）
    let completes: Vec<_> = collect
        .iter()
        .filter(|e| matches!(e, StreamEvent::AssistantTurnComplete { .. }))
        .collect();
    assert_eq!(
        completes.len(),
        2,
        "turn counter was reset, so continuation can use all max_turns=2 turns"
    );
    // 最终 Idle（channel 关闭 → Completed）
    assert!(matches!(kernel.state(), AgentState::Completed));
}
