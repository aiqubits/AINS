//! Phase 1 工具循环单测 + 完整 Agent Loop 集成测试（Native）。
//!
//! 用例对照 Harness `tests/test_engine/test_query_engine.py`：
//! plain text / 工具循环 / 工具异常合成 error result / 未知工具 / max_turns /
//! retry 状态上报 / 空 assistant 丢弃 / sanitize 悬空 tool_use / continue_pending。

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use serde_json::{Value, json};

use rust_agent::TokioRuntimeAdapter;
use rust_agent::error::ToolError;
use rust_agent::hooks::{
    HookDefinition, HookEvent, HookExecutor, HookRegistry, PromptHookDefinition,
};
use rust_agent::kernel::{
    AgentEvent, AgentKernel, AgentKernelConfig, AgentState, ContentBlock, ConversationMessage,
    QUERY_INTERRUPTED_STATUS, Role, ScriptedModelClient, StreamEvent, SystemEventType,
};
use rust_agent::model_client::{
    EventStream, ModelClient, ModelRequest, ModelStreamEvent, UsageSnapshot,
};
use rust_agent::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult, ToolRuntime};

struct EchoTool;

/// 永久静默的模型流：回归 Stop 必须能唤醒正在等待网络事件的模型 turn，
/// 而不是只能等底层 HTTP 超时。
struct PendingModelClient {
    started: Arc<AtomicBool>,
}

/// 在模型完整返回后、工具批分发前请求中断的流。它让测试精确覆盖
/// `ExecutingTools` 的边界，而不是模型请求开始前的中断分支。
struct InterruptAfterCompleteStream {
    event: Option<ModelStreamEvent>,
    interrupt: Arc<AtomicBool>,
}

impl futures::Stream for InterruptAfterCompleteStream {
    type Item = ModelStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.event.take() {
            Poll::Ready(Some(event))
        } else {
            self.interrupt.store(true, Ordering::SeqCst);
            Poll::Ready(None)
        }
    }
}

struct InterruptAfterToolTurnClient {
    message: ConversationMessage,
    /// AgentKernel 在构造后创建中断 handle，测试随后将它接入这里。
    interrupt: Arc<std::sync::Mutex<Option<Arc<AtomicBool>>>>,
}

#[async_trait::async_trait]
impl ModelClient for InterruptAfterToolTurnClient {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, rust_agent::error::AgentError> {
        let interrupt = self
            .interrupt
            .lock()
            .expect("interrupt lock poisoned")
            .clone()
            .expect("kernel interrupt handle must be installed before running");
        Ok(Box::pin(InterruptAfterCompleteStream {
            event: Some(ModelStreamEvent::Complete {
                message: self.message.clone(),
                usage: usage(),
                stop_reason: None,
            }),
            interrupt,
        }))
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }
}

#[async_trait::async_trait]
impl ModelClient for PendingModelClient {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, rust_agent::error::AgentError> {
        self.started.store(true, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::pending()))
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }
}

/// 先发 Complete、随后流永久挂起的模型（模拟违反“Complete 后关闭”契约的
/// 自定义 ModelClient 实现）；验证 Kernel 收到 Complete 即结束 turn，不依赖
/// 流 EOF 或用户中断（review 修复回归）。
struct CompleteThenHungClient {
    message: ConversationMessage,
}

#[async_trait::async_trait]
impl ModelClient for CompleteThenHungClient {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, rust_agent::error::AgentError> {
        let complete = ModelStreamEvent::Complete {
            message: self.message.clone(),
            usage: usage(),
            stop_reason: None,
        };
        use futures::StreamExt;
        Ok(Box::pin(
            futures::stream::iter(vec![complete]).chain(futures::stream::pending()),
        ))
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, rust_agent::error::AgentError> {
        Err(rust_agent::error::AgentError::Model("not scripted".into()))
    }
}

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
        Ok(ToolResult {
            output: format!("echo: {text}"),
            is_error: false,
            metadata: json!({"echo_length": text.len()}),
        })
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

/// 长时工具：验证查询级取消标志注入（review 接线回归）。execute 轮询注入的
/// 标志最多 3 秒，观测到置位（UI 中断）后返回；未注入（回归）立即报错。
struct CancelAwareTool {
    cancel: Arc<std::sync::Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>>,
}

#[async_trait::async_trait]
impl Tool for CancelAwareTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "cancel_aware".into(),
            description: "long-running tool that observes the query cancel flag".into(),
            input_schema: json!({ "type": "object" }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        _input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let flag = self
            .cancel
            .lock()
            .expect("cancel state lock poisoned")
            .clone()
            .ok_or_else(|| ToolError::Execution("query cancel flag was not injected".into()))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            if tokio::time::Instant::now() > deadline {
                return Ok(ToolResult::err("cancel flag never set"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(ToolResult::ok("cancel observed"))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::System
    }

    fn set_query_cancel(&self, flag: Option<Arc<std::sync::atomic::AtomicBool>>) {
        *self.cancel.lock().expect("cancel state lock poisoned") = flag;
    }
}

#[tokio::test]
async fn interrupt_flag_reaches_in_flight_tool_as_query_cancel() {
    // review 接线回归：工具批运行中 UI 置位中断 → Kernel 注入的查询级取消
    // 标志被长时工具观测到（修复前 ShellRequest.cancel 恒 None，运行中命令
    // 只能等超时）。同时验证共享标志在工具运行期间不被边界 check-and-clear
    // 提前消费（dispatch_many await 中事件循环不检查 interrupt）。
    use std::sync::atomic::Ordering;

    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(None, "tu_cancel", "cancel_aware", json!({})),
            usage(),
        ),
        ScriptedModelClient::text_turn("done", usage()),
    ]));
    let cancel_state = Arc::new(std::sync::Mutex::new(None));
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![Box::new(CancelAwareTool {
            cancel: Arc::clone(&cancel_state),
        })],
        test_config(),
    );
    let interrupt = kernel.interrupt_handle();
    // 并行置位：kernel.run 驱动工具批的同时，UI 侧 50ms 后置位中断。
    let notifier = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        interrupt.store(true, Ordering::SeqCst);
    });
    event_tx.try_send(user_message("start")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();
    notifier.await.unwrap();

    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolExecutionCompleted {
                output,
                is_error: false,
                ..
            } if output == "cancel observed"
        )),
        "cancel flag must reach the in-flight tool: {events:?}"
    );
}

#[tokio::test]
async fn complete_then_hung_stream_still_finishes_the_turn() {
    // review 修复回归：Complete 是协议终止事件。自定义 ModelClient 若在
    // Complete 后保持连接（流不 EOF），Kernel 必须仍结束本轮（历史实现
    // 继续等流 EOF，turn 永久挂起，只能靠用户中断逃逸）。
    let model = Arc::new(CompleteThenHungClient {
        message: ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
        },
    });
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::new(model, vec![Box::new(EchoTool)], test_config());
    let run = tokio::spawn(async move {
        kernel.run().await.unwrap();
        kernel
    });
    event_tx.try_send(user_message("finish")).unwrap();
    drop(event_tx);

    let kernel = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("kernel hung waiting for stream EOF after Complete")
        .expect("kernel task panicked");
    assert!(matches!(kernel.state(), AgentState::Completed));
    // 无工具的回答已落定：完整 turn 结束，非中断（无 QUERY_INTERRUPTED_STATUS）。
    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::AssistantTurnComplete { .. })),
        "turn must complete normally: {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, StreamEvent::Status { message } if message == QUERY_INTERRUPTED_STATUS)
        ),
        "no interrupt expected: {events:?}"
    );
}

#[tokio::test]
async fn stale_interrupt_during_complete_tail_does_not_abort_next_query() {
    // review 修复回归：用户在 Complete 尾窗（流关闭/超时窗口）内点击停止，
    // 回合已自然完成（无工具回答），该中断应为 no-op。若标志不被消费会残留，
    // Querying 入口把陈旧标志误判为新查询的中断（QUERY_INTERRUPTED_STATUS）。
    // 修复：主循环进入 Idle 时统一消费残留标志。
    let model = Arc::new(CompleteThenHungClient {
        message: ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
        },
    });
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::new(model, vec![Box::new(EchoTool)], test_config());
    let interrupt = kernel.interrupt_handle();
    let run = tokio::spawn(async move {
        kernel.run().await.unwrap();
        kernel
    });
    event_tx.try_send(user_message("first")).unwrap();

    // 第一轮：等 Kernel 处理 Complete 进入尾窗（500ms 收尾窗口）后置位中断，
    // 模拟用户在该窗口内点击停止——回合已自然完成，此中断不应影响后续查询。
    tokio::time::sleep(Duration::from_millis(100)).await;
    interrupt.store(true, Ordering::SeqCst);
    // 等尾窗超时 + 第一轮自然结束回 Idle。
    tokio::time::sleep(Duration::from_millis(700)).await;

    // 第二轮：若残留标志未被 Idle 入口消费，Querying 入口会中止本次查询。
    event_tx.try_send(user_message("second")).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(event_tx);

    let _kernel = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("kernel hung")
        .expect("kernel task panicked");
    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    // 两轮都必须自然完成：无中断状态，且出现两次 AssistantTurnComplete。
    assert!(
        !events.iter().any(
            |e| matches!(e, StreamEvent::Status { message } if message == QUERY_INTERRUPTED_STATUS)
        ),
        "stale interrupt flag must not abort the next query: {events:?}"
    );
    let completes = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::AssistantTurnComplete { .. }))
        .count();
    assert_eq!(
        completes, 2,
        "both turns must complete normally: {events:?}"
    );
}

#[tokio::test]
async fn interrupt_wakes_silent_model_stream() {
    let started = Arc::new(AtomicBool::new(false));
    let model = Arc::new(PendingModelClient {
        started: Arc::clone(&started),
    });
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::new(model, vec![Box::new(EchoTool)], test_config());
    let interrupt = kernel.interrupt_handle();
    let run = tokio::spawn(async move {
        kernel.run().await.unwrap();
        kernel
    });
    event_tx.try_send(user_message("hang")).unwrap();

    // Wait until the model request is actually in-flight so the test does not
    // accidentally exercise only the pre-turn boundary check.
    tokio::time::timeout(Duration::from_secs(1), async {
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("model stream did not start");
    interrupt.store(true, Ordering::SeqCst);
    drop(event_tx);

    let kernel = tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .expect("silent model stream ignored interrupt")
        .expect("kernel task panicked");
    assert!(matches!(kernel.state(), AgentState::Completed));
    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Status { message } if message == QUERY_INTERRUPTED_STATUS
    )));
}

#[tokio::test]
async fn interrupt_before_tool_dispatch_backfills_interrupted_tool_results() {
    let model = Arc::new(InterruptAfterToolTurnClient {
        message: ScriptedModelClient::assistant_tool_use(
            None,
            "tu_interrupted",
            "echo",
            json!({"text": "must not execute"}),
        ),
        interrupt: Arc::new(std::sync::Mutex::new(None)),
    });
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![Box::new(EchoTool)],
        test_config(),
    );
    *model.interrupt.lock().expect("interrupt lock poisoned") = Some(kernel.interrupt_handle());
    event_tx.try_send(user_message("start")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Status { message } if message == QUERY_INTERRUPTED_STATUS
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolExecutionStarted { .. })),
        "interrupted batch must not dispatch tools: {events:?}"
    );

    let conversation = &kernel.context().conversation;
    assert_eq!(conversation.len(), 3);
    assert!(matches!(
        &conversation[2].content[0],
        ContentBlock::ToolResult { tool_use_id, content, is_error: true, result_metadata }
            if tool_use_id == "tu_interrupted"
                && content == "interrupted"
                && result_metadata == &Value::Null
    ));
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

    fn is_read_only(&self, _input: &Value) -> bool {
        true
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

/// 只用于验证协议校验一定发生在工具分发之前。
struct SideEffectTool {
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for SideEffectTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "side_effect".into(),
            description: "increments an execution counter".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        _input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolResult::ok("executed"))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

struct OverlapTool {
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    max_in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for OverlapTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "overlap".into(),
            description: "records whether calls overlap".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"label": {"type": "string"}},
                "required": ["label"]
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
        use std::sync::atomic::Ordering;

        let label = input["label"].as_str().unwrap_or_default().to_string();
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        ctx.metadata.append_work_log(format!("overlap: {label}"));
        Ok(ToolResult::ok(label))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

#[tokio::test]
async fn default_constructor_denies_mutating_tools_without_permission_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("must-not-exist.txt");
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                None,
                "tu_write",
                "write_file",
                json!({"path": target, "content": "blocked"}),
            ),
            usage(),
        ),
        ScriptedModelClient::text_turn("write was blocked", usage()),
    ]));
    let (_kernel, events) = run_kernel(
        model,
        vec![Box::new(rust_agent::tools::filesystem::FileWriteTool)],
        AgentKernelConfig {
            cwd: dir.path().to_path_buf(),
            ..test_config()
        },
        vec![user_message("write a file")],
    )
    .await;

    assert!(!target.exists(), "safe constructor must not execute writes");
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolExecutionCompleted {
            tool_name,
            output,
            is_error: true,
            ..
        } if tool_name == "write_file" && output.contains("require user confirmation")
    )));
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
        StreamEvent::ToolExecutionCompleted {
            tool_use_id,
            output,
            is_error,
            metadata,
            tool_metadata,
            ..
        }
            if tool_use_id == "toolu_1"
                && output == "echo: ping"
                && !is_error
                && metadata == &json!({"echo_length": 4})
                && tool_metadata.work_log == vec!["echo: ping"]
    ));

    // user / assistant+tool_use / user+tool_result / assistant（对齐基线 len==4）
    let conversation = &kernel.context().conversation;
    assert_eq!(conversation.len(), 4);
    assert_eq!(conversation[2].role, Role::User);
    assert!(matches!(
        &conversation[2].content[0],
        ContentBlock::ToolResult { tool_use_id, content, is_error, result_metadata }
            if tool_use_id == "toolu_1"
                && content == "echo: ping"
                && !is_error
                && result_metadata == &json!({"echo_length": 4})
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
        ContentBlock::ToolResult { tool_use_id, content, is_error: true, .. }
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
async fn terminal_retry_surfaces_once_as_error_without_missing_final_message() {
    let model = Arc::new(ScriptedModelClient::new(vec![vec![
        ModelStreamEvent::Retry {
            message: "request failed: [no_active_plan] no active plan".into(),
            attempt: 4,
            max_attempts: 4,
            delay_secs: 0.0,
        },
    ]]));
    let (_, events) = run_kernel(model, vec![], test_config(), vec![user_message("hi")]).await;

    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Error { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "terminal retry must emit exactly one error: {events:?}"
    );
    assert!(errors[0].contains("no_active_plan"));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Status { .. })),
        "terminal retry must not be rendered as retry status: {events:?}"
    );
    assert!(
        !errors[0].contains("Model stream finished without a final message"),
        "terminal retry must not fall through to the generic missing-final error"
    );
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
                result_metadata: Value::Null,
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
                result_metadata: Value::Null,
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
async fn interrupt_flag_aborts_query_at_turn_boundary_and_returns_to_idle() {
    // 中断标志在模型 turn 边界协作式生效：运行前置位，内核在
    // Querying 起始 check-and-clear 命中 → 不查询模型、发 Status、回 Idle；
    // 通道随后关闭 → Completed。
    use std::sync::atomic::Ordering;

    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("should never be sent", usage()),
    ]));
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![Box::new(EchoTool)],
        test_config(),
    );
    let interrupt = kernel.interrupt_handle();
    event_tx
        .try_send(user_message("start a long task"))
        .unwrap();
    // 运行前置位：首个 Querying 边界即中断（新查询会先清旧标志，故需
    // 在 UserMessage 进入 Querying 后才生效——但 store 在 build 之后的
    // Querying drain 前：这里用另一种确定性方式）。
    interrupt.store(true, Ordering::SeqCst);
    drop(event_tx);
    kernel.run().await.unwrap();
    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }

    // 模型从未被调用（中断在首个 turn 前生效）
    assert!(
        model.recorded_requests().is_empty(),
        "interrupt must abort before any model call"
    );
    // 发出中断 Status
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Status { message } if message == QUERY_INTERRUPTED_STATUS
    )));
    // 无任何工具执行
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolExecutionStarted { .. }))
    );
    assert!(matches!(kernel.state(), AgentState::Completed));
}

#[tokio::test]
async fn interrupt_flag_is_consumed_on_read_so_next_query_runs() {
    // interrupt_requested 为 check-and-clear：第一个查询被中断并消耗标志，
    // 后续查询不受影响（标志已复位）。宿主另在发送前清标志防陈旧。
    use std::sync::atomic::Ordering;

    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("handled", usage()),
    ]));
    let (mut kernel, mut event_tx, _stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![Box::new(EchoTool)],
        test_config(),
    );
    let interrupt = kernel.interrupt_handle();
    // 两条消息：第一条被预置标志中断（swap 清标志），第二条正常查询。
    interrupt.store(true, Ordering::SeqCst);
    event_tx.try_send(user_message("first")).unwrap();
    event_tx.try_send(user_message("second")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    // first 被中断不查询；second 在标志已消耗后正常查询一次
    assert_eq!(
        model.recorded_requests().len(),
        1,
        "only 'second' reaches the model"
    );
    assert!(matches!(kernel.state(), AgentState::Completed));
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
async fn clear_conversation_discards_prior_context_before_the_next_prompt() {
    // The UI queues ClearConversation between turns.  The following prompt
    // must start a fresh model request rather than inherit the old user /
    // assistant history retained in the live kernel.
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("old reply", usage()),
        ScriptedModelClient::text_turn("fresh reply", usage()),
    ]));
    let (kernel, _events) = run_kernel(
        Arc::clone(&model),
        vec![Box::new(EchoTool)],
        test_config(),
        vec![
            user_message("old conversation"),
            AgentEvent::ClearConversation,
            user_message("fresh conversation"),
        ],
    )
    .await;

    let requests = model.recorded_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(requests[1].messages.len(), 1);
    assert_eq!(
        requests[1].messages[0],
        ConversationMessage::from_user_text("fresh conversation")
    );

    // Only the new turn remains in the live context after the channel closes.
    assert_eq!(kernel.context().conversation.len(), 2);
    assert_eq!(
        kernel.context().conversation[0],
        ConversationMessage::from_user_text("fresh conversation")
    );
}

#[tokio::test]
async fn lifecycle_hooks_fire_for_start_prompt_stop_and_end() {
    let mut registry = HookRegistry::new();
    for event in [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::Stop,
        HookEvent::SessionEnd,
    ] {
        registry.register(
            event,
            HookDefinition::Prompt(PromptHookDefinition {
                prompt: "$ARGUMENTS".into(),
                model: None,
                timeout_seconds: 5,
                matcher: None,
                block_on_failure: true,
                priority: 0,
            }),
        );
    }
    let hook_model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn(r#"{"ok":true}"#, usage()),
        ScriptedModelClient::text_turn(r#"{"ok":true}"#, usage()),
        ScriptedModelClient::text_turn(r#"{"ok":true}"#, usage()),
        ScriptedModelClient::text_turn(r#"{"ok":true}"#, usage()),
    ]));
    let hooks = Arc::new(
        HookExecutor::new(registry, std::env::temp_dir())
            .with_model(Arc::clone(&hook_model) as Arc<_>, None),
    );
    let runtime = ToolRuntime::new().with_hooks(hooks);
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("done", usage()),
    ]));
    let (mut kernel, mut event_tx, _stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::with_runtime(model, runtime, test_config());
    event_tx
        .try_send(AgentEvent::SystemEvent {
            event_type: SystemEventType::Startup,
        })
        .unwrap();
    event_tx.try_send(user_message("hello")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    let requests = hook_model.recorded_requests();
    assert_eq!(requests.len(), 4);
    let prompts: Vec<String> = requests
        .iter()
        .map(|request| request.messages[0].text())
        .collect();
    for event in ["session_start", "user_prompt_submit", "stop", "session_end"] {
        assert!(
            prompts.iter().any(|prompt| prompt.contains(event)),
            "{prompts:?}"
        );
    }
    assert!(prompts.iter().all(|prompt| prompt.contains("\"cwd\"")));
    let stop_payload = prompts
        .iter()
        .find(|prompt| prompt.contains("\"event\":\"stop\""))
        .expect("stop hook payload");
    assert!(stop_payload.contains("\"stop_reason\":\"tool_uses_empty\""));
}

#[tokio::test]
async fn user_prompt_hook_blocks_before_context_and_model() {
    let mut registry = HookRegistry::new();
    registry.register(
        HookEvent::UserPromptSubmit,
        HookDefinition::Prompt(PromptHookDefinition {
            prompt: "$ARGUMENTS".into(),
            model: None,
            timeout_seconds: 5,
            matcher: None,
            block_on_failure: true,
            priority: 0,
        }),
    );
    let hook_model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn(r#"{"ok":false,"reason":"blocked prompt"}"#, usage()),
    ]));
    let hooks = Arc::new(
        HookExecutor::new(registry, std::env::temp_dir())
            .with_model(Arc::clone(&hook_model) as Arc<_>, None),
    );
    let runtime = ToolRuntime::new().with_hooks(hooks);
    let model = Arc::new(ScriptedModelClient::new(vec![]));
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::with_runtime(
            Arc::clone(&model) as Arc<_>,
            runtime,
            test_config(),
        );
    event_tx.try_send(user_message("secret")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    assert!(kernel.context().conversation.is_empty());
    assert!(model.recorded_requests().is_empty());
    assert!(matches!(
        stream_rx.try_recv(),
        Ok(StreamEvent::Error { message, recoverable: true }) if message == "blocked prompt"
    ));
}

#[tokio::test]
async fn multi_tool_use_executes_concurrently_and_aggregates_in_original_order() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // 单轮 assistant 消息携带两个 tool_use → 并发执行 → 结果按原始顺序
    // 聚合成一条 user 消息（对齐 Harness gather 语义）。
    let assistant = ConversationMessage {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "需要两个工具".into(),
            },
            ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "overlap".into(),
                input: json!({"label": "hello"}),
            },
            ContentBlock::ToolUse {
                id: "toolu_2".into(),
                name: "overlap".into(),
                input: json!({"label": "world"}),
            },
        ],
    };
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(assistant.clone(), usage()),
        ScriptedModelClient::text_turn("两个工具都跑完了", usage()),
    ]));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let (kernel, events) = run_kernel(
        model,
        vec![Box::new(OverlapTool {
            in_flight,
            max_in_flight: Arc::clone(&max_in_flight),
        })],
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
            tool_use_id: "toolu_1".into(),
            tool_name: "overlap".into(),
            tool_input: json!({"label": "hello"}),
        }
    );
    assert_eq!(
        started[1],
        &StreamEvent::ToolExecutionStarted {
            tool_use_id: "toolu_2".into(),
            tool_name: "overlap".into(),
            tool_input: json!({"label": "world"}),
        }
    );
    assert_eq!(max_in_flight.load(Ordering::SeqCst), 2);

    // 会话结构：user / assistant(含 tool_use) / user(聚合两个 tool_result) / assistant
    let conversation = &kernel.context().conversation;
    assert_eq!(conversation.len(), 4);
    // 第 3 条（index 2）是单条 user 消息，包含两个 tool_result
    let results_msg = &conversation[2];
    assert_eq!(results_msg.role, Role::User);
    assert_eq!(results_msg.content.len(), 2);
    assert!(matches!(&results_msg.content[0], ContentBlock::ToolResult {
        tool_use_id, content, is_error: false, ..
    } if tool_use_id == "toolu_1" && content == "hello"));
    assert!(matches!(&results_msg.content[1], ContentBlock::ToolResult {
        tool_use_id, content, is_error: false, ..
    } if tool_use_id == "toolu_2" && content == "world"));
    assert_eq!(
        kernel.context().tool_metadata.work_log,
        vec!["overlap: hello", "overlap: world"]
    );
}

async fn assert_invalid_tool_use_batch_is_rejected(ids: &[&str], expected_reason: &str) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let assistant = ConversationMessage {
        role: Role::Assistant,
        content: ids
            .iter()
            .map(|id| ContentBlock::ToolUse {
                id: (*id).into(),
                name: "side_effect".into(),
                input: json!({}),
            })
            .collect(),
    };
    let model = Arc::new(ScriptedModelClient::new(vec![ScriptedModelClient::turn(
        assistant,
        usage(),
    )]));
    let executions = Arc::new(AtomicUsize::new(0));
    let (kernel, events) = run_kernel(
        Arc::clone(&model),
        vec![Box::new(SideEffectTool {
            executions: Arc::clone(&executions),
        })],
        test_config(),
        vec![user_message("run malformed batch")],
    )
    .await;

    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "no tool in an invalid batch may execute"
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        StreamEvent::ToolExecutionStarted { .. }
            | StreamEvent::ToolExecutionCompleted { .. }
            | StreamEvent::AssistantTurnComplete { .. }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Error {
            message,
            recoverable: true,
        } if message.contains("invalid tool_use batch") && message.contains(expected_reason)
    )));
    assert_eq!(
        kernel.context().conversation.len(),
        1,
        "the malformed assistant turn must not leave a dangling tool_use"
    );
    assert_eq!(model.recorded_requests().len(), 1);
}

#[tokio::test]
async fn duplicate_tool_use_ids_reject_entire_batch_before_dispatch() {
    assert_invalid_tool_use_batch_is_rejected(&["duplicate", "duplicate"], "must be unique").await;
}

#[tokio::test]
async fn empty_tool_use_ids_reject_entire_batch_before_dispatch() {
    assert_invalid_tool_use_batch_is_rejected(&["valid-sibling", ""], "must not be empty").await;
    assert_invalid_tool_use_batch_is_rejected(&["valid-sibling", " \t"], "must not be empty").await;
}

#[tokio::test]
async fn failing_tool_in_parallel_batch_does_not_drop_sibling_result() {
    let assistant = ConversationMessage {
        role: Role::Assistant,
        content: vec![
            ContentBlock::ToolUse {
                id: "toolu_fail".into(),
                name: "boom".into(),
                input: json!({}),
            },
            ContentBlock::ToolUse {
                id: "toolu_ok".into(),
                name: "echo".into(),
                input: json!({"text": "survived"}),
            },
        ],
    };
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(assistant, usage()),
        ScriptedModelClient::text_turn("done", usage()),
    ]));
    let (kernel, _) = run_kernel(
        model,
        vec![Box::new(FailingTool), Box::new(EchoTool)],
        test_config(),
        vec![user_message("run both")],
    )
    .await;

    let results = &kernel.context().conversation[2].content;
    assert_eq!(results.len(), 2);
    assert!(matches!(&results[0], ContentBlock::ToolResult {
        tool_use_id, is_error: true, ..
    } if tool_use_id == "toolu_fail"));
    assert!(matches!(&results[1], ContentBlock::ToolResult {
        tool_use_id, content, is_error: false, ..
    } if tool_use_id == "toolu_ok" && content == "echo: survived"));
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
                result_metadata: Value::Null,
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

#[tokio::test]
async fn system_prompt_carries_live_permission_mode_section() {
    use rust_agent::policy::{PermissionEngine, PermissionMode, PermissionSettings};
    use rust_agent::tools::interact::EnterPlanModeTool;

    // 引擎 Default 起步；模型第一轮调用 enter_plan_mode（只读、免确认）
    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let mut runtime = ToolRuntime::new();
    runtime.register(Box::new(EnterPlanModeTool::new(Arc::clone(&engine))));
    let runtime = runtime.with_permissions(Arc::clone(&engine), None);

    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(None, "tu_plan", "enter_plan_mode", json!({})),
            usage(),
        ),
        ScriptedModelClient::text_turn("planning now", usage()),
    ]));

    let (mut kernel, mut event_tx, _stream_rx) = AgentKernel::<TokioRuntimeAdapter>::with_runtime(
        model.clone(),
        runtime,
        AgentKernelConfig {
            system_prompt: Some("base instructions".into()),
            ..test_config()
        },
    );
    event_tx
        .try_send(user_message("please plan first"))
        .unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    let requests = model.recorded_requests();
    assert_eq!(requests.len(), 2);
    // 第一轮：宿主基础提示在前 + Default 模式段动态拼接
    let first = requests[0].system_prompt.as_deref().unwrap();
    assert!(first.starts_with("base instructions"));
    assert!(first.contains("Default permission mode is enabled"));
    // enter_plan_mode 于第一轮执行后，第二轮请求即携带 Plan 段（事先指引，
    // 减少"试错→被拒→再退出"轮次）
    let second = requests[1].system_prompt.as_deref().unwrap();
    assert!(second.contains("Plan mode is enabled"));
    assert!(second.contains("Do not call mutating tools"));
}

#[tokio::test]
async fn snapshot_with_dangling_tool_use_roundtrips_and_next_query_succeeds() {
    use rust_agent::context::{SessionSaveInput, SessionStore};
    use rust_agent::memory::{KvStore, RedbBackend, TABLE_KV};

    // 崩溃现场端到端：宿主在 AssistantTurnComplete 时持久化快照，
    // 此时工具尚未完成 → 快照含未配对 tool_use。重启后
    // load_current 必须给出已 sanitize 的历史，种子进 Kernel 后
    // 续问仍能正常完成（此前仅有 save/load 与 kernel 各自的分段覆盖）。
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(
        RedbBackend::open(dir.path().join("session.redb"))
            .expect("open redb")
            .table(TABLE_KV),
    );
    let store = SessionStore::new(Arc::clone(&kv));
    let crash_scene = vec![
        ConversationMessage::from_user_text("run echo for me"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "让我调用工具".into(),
                },
                ContentBlock::ToolUse {
                    id: "toolu_crash".into(),
                    name: "echo".into(),
                    input: json!({"text": "x"}),
                },
            ],
        },
    ];
    store
        .save(SessionSaveInput {
            cwd: "/proj/crash".into(),
            messages: crash_scene,
            ..Default::default()
        })
        .await
        .unwrap();

    // 回载：悬空 tool_use 整轮剪除，仅剩 user 消息
    let snapshot = store.load_current("/proj/crash").await.unwrap().unwrap();
    assert_eq!(snapshot.messages.len(), 1);
    assert_eq!(snapshot.messages[0].role, Role::User);

    // 种子进 Kernel（宿主恢复路径）后续问成功
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("recovered", usage()),
    ]));
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![Box::new(EchoTool)],
        test_config(),
    );
    kernel.context_mut().conversation = snapshot.messages;
    event_tx.try_send(user_message("continue please")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::AssistantTurnComplete { message, .. } if message.text() == "recovered"
    )));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StreamEvent::Error { .. })),
        "restored history must not trip protocol validation: {events:?}"
    );
    // 旧 user + 新 user + assistant；无悬空 tool_use 残留
    assert_eq!(kernel.context().conversation.len(), 3);
    assert!(!kernel.context().conversation.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "toolu_crash"))
    }));
}

#[tokio::test]
async fn oversized_tool_batch_is_rejected_whole() {
    // review 修复回归：无上限工具批会让模型（协议信任面内不可信输入）
    // 单轮派生同数量 hook 进程 / 并发 execute 与事件洪泛（资源耗尽面）。
    // 超限批次整批拒绝 + turn 忽略，不执行任何工具。
    let content: Vec<ContentBlock> = (0..65)
        .map(|i| ContentBlock::ToolUse {
            id: format!("tu_{i:02}"),
            name: "echo".into(),
            input: json!({}),
        })
        .collect();
    let model = Arc::new(ScriptedModelClient::new(vec![ScriptedModelClient::turn(
        ConversationMessage {
            role: Role::Assistant,
            content,
        },
        usage(),
    )]));
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<_>,
        vec![Box::new(EchoTool)],
        test_config(),
    );
    event_tx.try_send(user_message("big batch")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();
    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    // 整批拒绝：错误事件 + 无任何工具执行
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::Error { message, .. } if message.contains("invalid tool_use batch")
        )),
        "oversized batch must be rejected: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolExecutionStarted { .. })),
        "no tool may execute from an oversized batch: {events:?}"
    );
}

#[tokio::test]
async fn interrupt_at_tool_boundary_fires_stop_hook() {
    // review 修复回归：中断中止回合同样必须触发 Stop hook（与自然完成
    // 对称，stop_reason=interrupted），供宿主做轮次收尾（计费/日志/状态
    // 持久化）；历史三条中断路径均不执行 Stop hook，中断轮收尾缺失。
    let mut registry = HookRegistry::new();
    registry.register(
        HookEvent::Stop,
        HookDefinition::Prompt(PromptHookDefinition {
            prompt: "$ARGUMENTS".into(),
            model: None,
            timeout_seconds: 5,
            matcher: None,
            block_on_failure: true,
            priority: 0,
        }),
    );
    let hook_model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn(r#"{"ok":true}"#, usage()),
    ]));
    let hooks = Arc::new(
        HookExecutor::new(registry, std::env::temp_dir())
            .with_model(Arc::clone(&hook_model) as Arc<_>, None),
    );
    let runtime = ToolRuntime::new().with_hooks(hooks);
    let model = Arc::new(InterruptAfterToolTurnClient {
        message: ScriptedModelClient::assistant_tool_use(None, "tu_1", "echo", json!({})),
        interrupt: Arc::new(std::sync::Mutex::new(None)),
    });
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::with_runtime(
            Arc::clone(&model) as Arc<_>,
            runtime,
            test_config(),
        );
    // 模型流在 Complete 后置中断：工具批分发前命中 ExecutingTools 中断分支。
    *model.interrupt.lock().unwrap() = Some(kernel.interrupt_handle());
    event_tx.try_send(user_message("run")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();
    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    // 中断 Status 已发出（回合被中止）
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::Status { message } if message == QUERY_INTERRUPTED_STATUS
        )),
        "{events:?}"
    );
    // Stop hook 已触发且 stop_reason=interrupted
    let requests = hook_model.recorded_requests();
    let stop_payload = requests
        .iter()
        .find(|request| request.messages[0].text().contains("\"event\":\"stop\""))
        .expect("interrupt must fire the Stop hook");
    assert!(
        stop_payload.messages[0]
            .text()
            .contains("\"stop_reason\":\"interrupted\""),
        "{stop_payload:?}"
    );
}

#[tokio::test]
async fn disabled_tool_excluded_from_model_context_and_rejected_on_call() {
    // 工具活跃状态双保险集成回归：
    // 1) 禁用 echo 后，Kernel 每轮 ModelRequest.tools 不含 echo（模型上下文过滤）；
    // 2) 模型仍输出 echo 的 tool_use 时，执行层 fail-closed 返回 is_error，
    //    且会话继续存活（脚本第二段文本回答正常落地）。
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(None, "tu_echo", "echo", json!({"text": "x"})),
            usage(),
        ),
        ScriptedModelClient::text_turn("done", usage()),
    ]));
    let mut runtime = ToolRuntime::new();
    runtime.register(Box::new(EchoTool));
    runtime.set_tool_enabled("echo", false);
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::with_runtime(
            Arc::clone(&model) as Arc<_>,
            runtime,
            test_config(),
        );
    event_tx.try_send(user_message("start")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    // 模型上下文过滤：所有轮次的工具清单都不得包含被禁工具
    let requests = model.recorded_requests();
    assert!(!requests.is_empty(), "kernel must have queried the model");
    for request in &requests {
        let names: Vec<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(
            !names.contains(&"echo"),
            "disabled tool leaked into model context: {names:?}"
        );
    }

    // 执行兜底：echo 的 tool_use 被拒绝为 is_error 的 tool_result
    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolExecutionCompleted {
                tool_name,
                is_error: true,
                ..
            } if tool_name == "echo"
        )),
        "disabled tool dispatch must fail closed: {events:?}"
    );
    // 会话存活：后续文本回答已落地（完整 turn 结束）
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::AssistantTurnComplete { .. })),
        "kernel must continue after disabled-tool rejection: {events:?}"
    );
}

/// 会话进行中翻转共享禁用集合的工具：宿主（/tools 面板）修改共享 Arc 后，
/// Kernel 下一轮 api_schemas 应立即感知（无需重启会话）。
struct FlipperTool {
    disabled: Arc<std::sync::RwLock<HashSet<String>>>,
}

#[async_trait::async_trait]
impl Tool for FlipperTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "flipper".into(),
            description: "flip echo availability in the shared disabled set".into(),
            input_schema: json!({"type": "object", "properties": {"enable": {"type": "boolean"}}}),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let enable = input
            .get("enable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut guard = self
            .disabled
            .write()
            .expect("shared disabled lock poisoned");
        if enable {
            guard.remove("echo");
        } else {
            guard.insert("echo".to_string());
        }
        Ok(ToolResult::ok(format!("echo enabled={enable}")))
    }
}

fn request_tool_names(request: &ModelRequest) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

#[tokio::test]
async fn shared_disabled_source_flips_live_kernel_context_and_execution() {
    // share_disabled 注入的共享源在会话进行中被外部（宿主面板等价物）修改：
    // - turn1 flipper(disable) 后，turn2 的模型请求上下文不再含 echo；
    // - turn2 flipper(enable) 后，turn3 的上下文恢复包含 echo 且可正常执行。
    // 覆盖“会话进行中重新启用工具、下一轮上下文恢复包含”的方向。
    let shared: Arc<std::sync::RwLock<HashSet<String>>> = Arc::new(Default::default());
    let mut runtime = ToolRuntime::new();
    runtime.register(Box::new(EchoTool));
    runtime.register(Box::new(FlipperTool {
        disabled: Arc::clone(&shared),
    }));
    // 集成测试无持久化记账语义（无 ToolStateService/dirty），observer 传
    // None；仅验证共享源翻转。生产装配在 service.rs 注入 dirty 递增回调。
    runtime.share_disabled(Arc::clone(&shared), None);

    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                None,
                "tu_f1",
                "flipper",
                json!({"enable": false}),
            ),
            usage(),
        ),
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                None,
                "tu_f2",
                "flipper",
                json!({"enable": true}),
            ),
            usage(),
        ),
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(None, "tu_e", "echo", json!({"text": "hi"})),
            usage(),
        ),
        ScriptedModelClient::text_turn("done", usage()),
    ]));
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::with_runtime(
            Arc::clone(&model) as Arc<_>,
            runtime,
            test_config(),
        );
    event_tx.try_send(user_message("start")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    // 上下文过滤随共享源翻转：turn1 含 echo → turn2 不含 → turn3 恢复
    let requests = model.recorded_requests();
    assert!(
        requests.len() >= 3,
        "expected at least 3 model turns, got {}",
        requests.len()
    );
    assert!(request_tool_names(&requests[0]).contains(&"echo"));
    assert!(
        !request_tool_names(&requests[1]).contains(&"echo"),
        "turn2 must exclude echo after flipper disabled it: {:?}",
        request_tool_names(&requests[1])
    );
    assert!(
        request_tool_names(&requests[2]).contains(&"echo"),
        "turn3 must include echo after re-enable: {:?}",
        request_tool_names(&requests[2])
    );

    // 执行层：echo 在 turn3 重新启用后正常执行成功
    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolExecutionCompleted {
                tool_name,
                is_error: false,
                ..
            } if tool_name == "echo"
        )),
        "re-enabled echo must execute successfully: {events:?}"
    );
}

#[tokio::test]
async fn disabled_tool_dispatch_skips_pre_tool_use_hook() {
    // 禁用检查位于 pre_tool_use hook 之前（review 建议补测）：被禁工具
    // 不得触发任何 hook 副作用——若 hook 先于禁用检查执行，观测/拦截类
    // hook（如审计、安全过滤）会为从未执行的工具产生事件，且禁用状态
    // 的语义被 hook 结果污染。用记录请求的 prompt hook model 断言：
    // echo 被禁用后 dispatch 直接拒绝，hook model 零请求。
    let mut registry = HookRegistry::new();
    let hook_model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn(r#"{"ok":true}"#, usage()),
    ]));
    registry.register(
        HookEvent::PreToolUse,
        HookDefinition::Prompt(PromptHookDefinition {
            prompt: "$ARGUMENTS".into(),
            model: None,
            timeout_seconds: 5,
            matcher: None,
            block_on_failure: false,
            priority: 0,
        }),
    );
    let hooks = Arc::new(
        HookExecutor::new(registry, std::env::temp_dir())
            .with_model(Arc::clone(&hook_model) as Arc<_>, None),
    );
    let mut runtime = ToolRuntime::new().with_hooks(hooks);
    runtime.register(Box::new(EchoTool));
    runtime.set_tool_enabled("echo", false);

    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                None,
                "tu_echo",
                "echo",
                json!({ "text": "x" }),
            ),
            usage(),
        ),
        ScriptedModelClient::text_turn("done", usage()),
    ]));
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::with_runtime(
            Arc::clone(&model) as Arc<_>,
            runtime,
            test_config(),
        );
    event_tx.try_send(user_message("start")).unwrap();
    drop(event_tx);
    kernel.run().await.unwrap();

    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolExecutionCompleted {
                tool_name,
                is_error: true,
                ..
            } if tool_name == "echo"
        )),
        "disabled echo must be rejected by dispatch: {events:?}"
    );
    assert!(
        hook_model.recorded_requests().is_empty(),
        "pre_tool_use hook must not fire for a disabled tool: {:?}",
        hook_model.recorded_requests()
    );
}
