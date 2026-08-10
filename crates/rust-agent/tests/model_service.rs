//! Phase 5.2 集成测试：GatewayModelClient 端到端（wiremock Mock AI Gateway）。
//!
//! 覆盖：SSE 流式 delta/Complete、工具协议解析与 UI 过滤、可重试失败的
//! 流内 Retry 事件、不可重试失败终止、embed/stt/tts 直连能力。

#![cfg(not(target_arch = "wasm32"))]

use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use serde_json::json;
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use client_api::{Client, ClientConfig};
use rust_agent::TokioRuntimeAdapter;
use rust_agent::kernel::messages::{ContentBlock, ConversationMessage, Role};
use rust_agent::model_client::{ModelClient, ModelRequest, ModelStreamEvent};
use rust_agent::model_service::GatewayModelClient;

fn gateway_client(mock_uri: &str) -> GatewayModelClient<TokioRuntimeAdapter> {
    let config = ClientConfig::new(mock_uri)
        .with_max_retries(0) // 传输层重连关闭：重试语义由 ModelClient 层验证
        .with_timeout(10)
        .with_no_proxy(true);
    let client = Client::new(config).expect("valid config");
    client.set_token("test-jwt");
    GatewayModelClient::new(client)
}

fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(body, "text/event-stream")
}

fn completed_sse(deltas: &[&str], full_text: &str) -> String {
    let mut body = String::from(
        "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
    );
    for (i, delta) in deltas.iter().enumerate() {
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            json!({
                "type": "response.output_text.delta", "item_id": "msg_1",
                "output_index": 0, "content_index": 0,
                "delta": delta, "sequence_number": i + 1
            })
        ));
    }
    let response = json!({
        "id": "resp_1", "object": "response", "created_at": 1,
        "model": "gpt-test", "capability": "chat", "status": "completed",
        "incomplete_details": null,
        "output": [{
            "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": full_text, "annotations": []}]
        }],
        "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10},
        "error": null
    });
    body.push_str(&format!(
        "event: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed", "response": response, "sequence_number": 99})
    ));
    body
}

async fn collect_events(
    client: &GatewayModelClient<TokioRuntimeAdapter>,
    request: ModelRequest,
) -> Vec<ModelStreamEvent> {
    let mut stream = client.stream_response(request).await.expect("stream");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn test_stream_response_deltas_and_complete() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(sse_response(completed_sse(&["你好", "世界"], "你好世界")))
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            messages: vec![ConversationMessage::from_user_text("hi")],
            ..Default::default()
        },
    )
    .await;

    let deltas: String = events
        .iter()
        .filter_map(|e| match e {
            ModelStreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, "你好世界");
    let Some(ModelStreamEvent::Complete {
        message,
        usage,
        stop_reason,
    }) = events.last()
    else {
        panic!("expected Complete, got {:?}", events.last());
    };
    assert_eq!(message.text(), "你好世界");
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.output_tokens, 3);
    assert!(stop_reason.is_none());
}

#[tokio::test]
async fn test_stream_response_parses_tool_use_and_filters_ui_deltas() {
    let mock_server = MockServer::start().await;
    let full_text = "我来算一下。\n<tool_use id=\"call_1\" name=\"calculator\">\n{\"expression\": \"1+1\"}\n</tool_use>";
    let deltas = [
        "我来算一下。\n<tool_use id=\"call_1\" na",
        "me=\"calculator\">\n{\"expression\": \"1+1\"}\n</tool_use>",
    ];
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(sse_response(completed_sse(&deltas, full_text)))
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            messages: vec![ConversationMessage::from_user_text("1+1=?")],
            tools: vec![rust_agent::tools::ToolDef {
                name: "calculator".into(),
                description: "calc".into(),
                input_schema: json!({"type": "object"}),
            }],
            ..Default::default()
        },
    )
    .await;

    let visible: String = events
        .iter()
        .filter_map(|e| match e {
            ModelStreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(!visible.contains("<tool_use"));
    assert!(visible.contains("我来算一下。"));

    let Some(ModelStreamEvent::Complete { message, .. }) = events.last() else {
        panic!("expected Complete");
    };
    let tool_uses = message.tool_uses();
    assert_eq!(tool_uses.len(), 1);
    assert_eq!(tool_uses[0].id, "call_1");
    assert_eq!(tool_uses[0].name, "calculator");
    assert_eq!(tool_uses[0].input, json!({"expression": "1+1"}));
}

/// 首次 503、第二次成功的响应器。
struct FlakyResponder {
    calls: AtomicU32,
    success_body: String,
}

impl Respond for FlakyResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(503).set_body_json(json!({
                "id": "resp_f", "object": "response", "created_at": 1,
                "model": null, "capability": null, "status": "failed",
                "incomplete_details": null, "output": [], "usage": null,
                "error": {"code": "service_unavailable",
                           "message": "AI provider request failed"}
            }))
        } else {
            sse_response(self.success_body.clone())
        }
    }
}

#[tokio::test]
async fn test_retryable_failure_emits_retry_event_then_succeeds() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(FlakyResponder {
            calls: AtomicU32::new(0),
            success_body: completed_sse(&["恢复"], "恢复"),
        })
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            messages: vec![ConversationMessage::from_user_text("hi")],
            ..Default::default()
        },
    )
    .await;

    let retry = events
        .iter()
        .find_map(|e| match e {
            ModelStreamEvent::Retry {
                attempt,
                max_attempts,
                ..
            } => Some((*attempt, *max_attempts)),
            _ => None,
        })
        .expect("retry event");
    assert_eq!(retry, (1, 4));
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::Complete { message, .. }) if message.text() == "恢复"
    ));
}

#[tokio::test]
async fn test_non_retryable_failure_terminates_without_complete() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "id": "resp_f", "object": "response", "created_at": 1,
            "model": null, "capability": null, "status": "failed",
            "incomplete_details": null, "output": [], "usage": null,
            "error": {"code": "no_active_plan", "message": "No active plan"}
        })))
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            messages: vec![ConversationMessage::from_user_text("hi")],
            ..Default::default()
        },
    )
    .await;

    assert_eq!(events.len(), 1);
    match &events[0] {
        ModelStreamEvent::Retry {
            attempt,
            max_attempts,
            message,
            ..
        } => {
            assert_eq!(attempt, max_attempts);
            assert!(message.contains("no_active_plan"), "message: {message}");
        }
        other => panic!("expected terminal Retry, got {other:?}"),
    }
}

#[tokio::test]
async fn test_embed_stt_tts_direct_capabilities() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({"capability": "embedding"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_e", "object": "response", "created_at": 1,
            "model": "embed", "capability": "embedding", "status": "completed",
            "incomplete_details": null,
            "output": [{"embedding": [0.5, 0.25], "index": 0, "dimensions": 2}],
            "usage": null, "error": null
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({"capability": "stt"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_s", "object": "response", "created_at": 1,
            "model": "stt", "capability": "stt", "status": "completed",
            "incomplete_details": null,
            "output": [{"type": "transcription", "text": "语音内容"}],
            "usage": null, "error": null
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({"capability": "tts"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_t", "object": "response", "created_at": 1,
            "model": "tts", "capability": "tts", "status": "completed",
            "incomplete_details": null,
            "output": [{"type": "audio", "data": "YWJj", "content_type": "audio/mpeg"}],
            "usage": null, "error": null
        })))
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());

    let vector = client.embed("你好").await.unwrap();
    assert_eq!(vector, vec![0.5f32, 0.25f32]);

    let text = client.stt(b"RIFF....WAVEdata").await.unwrap();
    assert_eq!(text, "语音内容");

    let audio = client.tts("你好").await.unwrap();
    assert_eq!(audio, b"abc");
}

#[tokio::test]
async fn test_request_carries_system_prompt_and_tool_protocol() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({"model": "gpt-test"})))
        .and(body_string_contains("base prompt"))
        .and(body_string_contains("Tool Call Protocol"))
        .and(body_string_contains("calculator"))
        .respond_with(sse_response(completed_sse(&["ok"], "ok")))
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            model: Some("gpt-test".into()),
            system_prompt: Some("base prompt".into()),
            messages: vec![ConversationMessage::from_user_text("hi")],
            tools: vec![rust_agent::tools::ToolDef {
                name: "calculator".into(),
                description: "calc".into(),
                input_schema: json!({"type": "object"}),
            }],
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::Complete { message, .. }) if message.text() == "ok"
    ));
}

#[tokio::test]
async fn test_history_tool_blocks_rendered_as_protocol_text() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_string_contains("tool_result id="))
        .respond_with(sse_response(completed_sse(&["2"], "答案是 2")))
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let history = vec![
        ConversationMessage::from_user_text("1+1=?"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "calculator".into(),
                input: json!({"expression": "1+1"}),
            }],
        },
        ConversationMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "2".into(),
                is_error: false,
                result_metadata: serde_json::Value::Null,
            }],
        },
    ];
    let events = collect_events(
        &client,
        ModelRequest {
            messages: history,
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::Complete { message, .. }) if message.text() == "答案是 2"
    ));
}

/// 首次返回指定部分 SSE（中途失败），重试后返回完整响应。
struct PartialThenFullResponder {
    calls: AtomicU32,
    first_body: String,
    full_body: String,
}

impl Respond for PartialThenFullResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            sse_response(self.first_body.clone())
        } else {
            sse_response(self.full_body.clone())
        }
    }
}

/// created + 指定 delta 序列 + 流内 error（中途失败，无终止事件）。
fn interrupted_sse(deltas: &[&str]) -> String {
    let mut body = String::from(
        "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
    );
    for (i, delta) in deltas.iter().enumerate() {
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            json!({
                "type": "response.output_text.delta", "item_id": "msg_1",
                "output_index": 0, "content_index": 0,
                "delta": delta, "sequence_number": i + 1
            })
        ));
    }
    body.push_str(
        "event: error\ndata: {\"type\":\"error\",\"code\":\"provider_stream_error\",\
         \"message\":\"mid-stream boom\",\"param\":null,\"sequence_number\":9}\n\n",
    );
    body
}

#[tokio::test]
async fn test_midstream_error_retries_and_dedups_visible_text() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(PartialThenFullResponder {
            calls: AtomicU32::new(0),
            first_body: interrupted_sse(&["Hello"]),
            full_body: completed_sse(&["Hello", " World"], "Hello World"),
        })
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            messages: vec![ConversationMessage::from_user_text("hi")],
            ..Default::default()
        },
    )
    .await;

    // 中途 error 触发一次流内 Retry
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelStreamEvent::Retry { .. })),
        "expected a Retry event after mid-stream error"
    );
    // 可见文本跨重试去重：不出现 "HelloHello"，最终恰为 "Hello World"
    let visible: String = events
        .iter()
        .filter_map(|e| match e {
            ModelStreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        visible, "Hello World",
        "visible deltas must be de-duplicated across retry"
    );
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::Complete { message, .. }) if message.text() == "Hello World"
    ));
}

/// 回归（非阻断建议 #1）：流中断于 `<tool_use>` 块内部时，旧实现每次
/// 重试重建 ToolTagFilter 但保留去重计数，导致重试后的新 gate 从 Normal
/// 态接收块中部内容，协议片段泄漏进 UI delta。修复后 gate 跨重试延续。
#[tokio::test]
async fn test_retry_inside_tool_use_block_does_not_leak_protocol() {
    let mock_server = MockServer::start().await;
    let head = "答案是 <tool_use id=\"c1\" name=\"calc\">{\"a\"";
    let tail = ":1}</tool_use> 完成";
    let full_text = format!("{head}{tail}");
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(PartialThenFullResponder {
            calls: AtomicU32::new(0),
            // 首次：恰好中断在 <tool_use> 块的 JSON 体内部
            first_body: interrupted_sse(&[head]),
            full_body: completed_sse(&[head, tail], &full_text),
        })
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            messages: vec![ConversationMessage::from_user_text("1+1=?")],
            ..Default::default()
        },
    )
    .await;

    let visible: String = events
        .iter()
        .filter_map(|e| match e {
            ModelStreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    // 协议片段（标签/JSON 体碎片）不得泄漏，可见文本为标签前后文本拼接
    assert!(!visible.contains("<tool_use"), "visible: {visible}");
    assert!(!visible.contains(":1}"), "visible: {visible}");
    assert_eq!(visible, "答案是  完成");
    // 最终消息仍正确解析出 ToolUse
    let Some(ModelStreamEvent::Complete { message, .. }) = events.last() else {
        panic!("expected Complete");
    };
    let tool_uses = message.tool_uses();
    assert_eq!(tool_uses.len(), 1);
    assert_eq!(tool_uses[0].input, json!({"a": 1}));
}

/// 回归（评审建议测试）：重试续传切分点落在多字节 CJK 序列中部时，
/// 去重按**字符**计数不得重复、丢字或撕裂字符（若按字节计数，重试
/// delta 以不同切分覆盖同一前缀时会在非字符边界切分 panic 或错位）。
#[tokio::test]
async fn test_midstream_retry_dedups_multibyte_unicode_without_corruption() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(PartialThenFullResponder {
            calls: AtomicU32::new(0),
            // 首次中断于 4 个 CJK 字符后（已发 4 字符 = 12 字节）
            first_body: interrupted_sse(&["你好，世"]),
            // 重试后以不同切分重发：第二个 delta 跨越既发/新增边界（“世”已发，
            // “界真大”为新增）
            full_body: completed_sse(&["你好，", "世界真大"], "你好，世界真大"),
        })
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            messages: vec![ConversationMessage::from_user_text("hi")],
            ..Default::default()
        },
    )
    .await;

    let visible: String = events
        .iter()
        .filter_map(|e| match e {
            ModelStreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        visible, "你好，世界真大",
        "CJK deltas must be de-duplicated by char count across retry"
    );
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::Complete { message, .. }) if message.text() == "你好，世界真大"
    ));
}

/// created + response.failed 终止事件（携带指定错误码）。
fn failed_sse(code: &str, message: &str) -> String {
    let response = json!({
        "id": "resp_f", "object": "response", "created_at": 1,
        "model": null, "capability": null, "status": "failed",
        "incomplete_details": null, "output": [], "usage": null,
        "error": {"code": code, "message": message}
    });
    format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"sequence_number\":0}}\n\n\
         event: response.failed\ndata: {}\n\n",
        json!({"type": "response.failed", "response": response, "sequence_number": 1})
    )
}

/// 回归（非阻断建议 #2）：流内 Failed 信封携带终态错误码（如
/// no_active_plan）时应立即终止，不得重试 3 次 + 退避（.expect(1)
/// 断言仅 1 次物理请求）。
#[tokio::test]
async fn test_non_retryable_in_stream_failure_terminates_immediately() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(sse_response(failed_sse("no_active_plan", "No active plan")))
        .expect(1)
        .mount(&mock_server)
        .await;

    let client = gateway_client(&mock_server.uri());
    let events = collect_events(
        &client,
        ModelRequest {
            messages: vec![ConversationMessage::from_user_text("hi")],
            ..Default::default()
        },
    )
    .await;

    assert_eq!(
        events.len(),
        1,
        "expected single terminal event, got {events:?}"
    );
    match &events[0] {
        ModelStreamEvent::Retry {
            attempt,
            max_attempts,
            message,
            ..
        } => {
            assert_eq!(
                attempt, max_attempts,
                "must be terminal, not a genuine retry"
            );
            assert!(message.contains("no_active_plan"), "message: {message}");
        }
        other => panic!("expected terminal Retry, got {other:?}"),
    }
}
