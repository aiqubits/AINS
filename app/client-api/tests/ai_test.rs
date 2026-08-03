//! AI 传输层集成测试（Phase 5.1）：`POST /api/ai/response` 统一 envelope、
//! 直连能力（embedding/stt/tts）、SSE 流式与失败信封映射。

#![cfg(not(target_arch = "wasm32"))]

use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use client_api::{
    AiContentPart, AiInput, AiInputMessage, AiRequest, AiStreamEvent, ChatOptions, Client,
    ClientConfig, ClientError, TtsOptions,
};

mod common;
use common::create_test_client;

fn chat_response_json(text: &str) -> serde_json::Value {
    json!({
        "id": "resp_1", "object": "response", "created_at": 1,
        "model": "gpt-test", "capability": "chat", "status": "completed",
        "incomplete_details": null,
        "output": [{
            "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
        "error": null
    })
}

#[tokio::test]
async fn test_chat_non_streaming_success() {
    let (client, mock_server) = create_test_client().await;
    client.set_token("test-jwt");

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(header("authorization", "Bearer test-jwt"))
        .and(body_partial_json(json!({
            "input": [{"role": "user", "content": "你好"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_json("你好！")))
        .mount(&mock_server)
        .await;

    let response = client
        .chat(
            vec![AiInputMessage::user_text("你好")],
            &ChatOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, "completed");
    assert_eq!(response.output_text(), "你好！");
    assert_eq!(response.usage.unwrap().total_tokens, 15);
}

#[tokio::test]
async fn test_chat_sends_instructions_and_options() {
    let (client, mock_server) = create_test_client().await;

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({
            "model": "gpt-test",
            "instructions": "you are a bot",
            "max_output_tokens": 128,
            "temperature": 0.5
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_json("ok")))
        .mount(&mock_server)
        .await;

    let options = ChatOptions {
        model: Some("gpt-test".into()),
        instructions: Some("you are a bot".into()),
        max_output_tokens: Some(128),
        temperature: Some(0.5),
        ..Default::default()
    };
    let response = client
        .chat(vec![AiInputMessage::user_text("hi")], &options)
        .await
        .unwrap();
    assert_eq!(response.output_text(), "ok");
}

#[tokio::test]
async fn test_vision_message_serializes_input_image_part() {
    let (client, mock_server) = create_test_client().await;

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "描述这张图"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
                ]
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_json("一张图")))
        .mount(&mock_server)
        .await;

    let messages = vec![AiInputMessage::user_parts(vec![
        AiContentPart::InputText {
            text: "描述这张图".into(),
        },
        AiContentPart::InputImage {
            image_url: "data:image/png;base64,AAAA".into(),
            detail: None,
        },
    ])];
    let response = client
        .chat(messages, &ChatOptions::default())
        .await
        .unwrap();
    assert_eq!(response.output_text(), "一张图");
}

#[tokio::test]
async fn test_embed_batch_returns_ordered_vectors() {
    let (client, mock_server) = create_test_client().await;

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({
            "capability": "embedding",
            "input": ["a", "b"]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_e", "object": "response", "created_at": 1,
            "model": "embed-model", "capability": "embedding", "status": "completed",
            "incomplete_details": null,
            "output": [
                {"embedding": [0.1, 0.2], "index": 0, "dimensions": 2},
                {"embedding": [0.3, 0.4], "index": 1, "dimensions": 2}
            ],
            "usage": {"input_tokens": 2, "output_tokens": 0, "total_tokens": 2},
            "error": null
        })))
        .mount(&mock_server)
        .await;

    let vectors = client
        .embed(vec!["a".into(), "b".into()], None)
        .await
        .unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0], vec![0.1f32, 0.2f32]);
    assert_eq!(vectors[1], vec![0.3f32, 0.4f32]);
}

#[tokio::test]
async fn test_stt_encodes_audio_and_extracts_transcription() {
    let (client, mock_server) = create_test_client().await;

    // b"abc" 的标准 base64 是 "YWJj"
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({
            "capability": "stt",
            "input": {"data": "YWJj", "format": "wav"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_s", "object": "response", "created_at": 1,
            "model": "stt-model", "capability": "stt", "status": "completed",
            "incomplete_details": null,
            "output": [{"type": "transcription", "text": "hello world"}],
            "usage": null, "error": null
        })))
        .mount(&mock_server)
        .await;

    let text = client.stt(b"abc", "wav", None).await.unwrap();
    assert_eq!(text, "hello world");
}

#[tokio::test]
async fn test_tts_sends_audio_object_and_decodes_output() {
    let (client, mock_server) = create_test_client().await;

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({
            "capability": "tts",
            "input": "读一下这句话",
            "audio": {"voice": "alloy", "format": "mp3", "speed": 1.0}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_t", "object": "response", "created_at": 1,
            "model": "tts-model", "capability": "tts", "status": "completed",
            "incomplete_details": null,
            "output": [{"type": "audio", "data": "YWJj", "content_type": "audio/mpeg"}],
            "usage": null, "error": null
        })))
        .mount(&mock_server)
        .await;

    let options = TtsOptions {
        format: Some("mp3".into()),
        speed: Some(1.0),
        ..Default::default()
    };
    let audio = client.tts("读一下这句话", "alloy", &options).await.unwrap();
    assert_eq!(audio.data, b"abc");
    assert_eq!(audio.content_type, "audio/mpeg");
}

#[tokio::test]
async fn test_failed_envelope_maps_to_api_error() {
    let (client, mock_server) = create_test_client().await;

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "id": "resp_f", "object": "response", "created_at": 1,
            "model": null, "capability": null, "status": "failed",
            "incomplete_details": null, "output": [], "usage": null,
            "error": {"code": "no_active_plan",
                       "message": "No active plan with remaining calls"}
        })))
        .mount(&mock_server)
        .await;

    let err = client
        .chat(
            vec![AiInputMessage::user_text("hi")],
            &ChatOptions::default(),
        )
        .await
        .unwrap_err();
    match err {
        ClientError::Api {
            status,
            code,
            message,
        } => {
            assert_eq!(status, 403);
            assert_eq!(code, "no_active_plan");
            assert!(message.contains("active plan"));
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn test_response_stream_yields_delta_and_terminal_events() {
    let (client, mock_server) = create_test_client().await;

    let completed_payload = json!({
        "type": "response.completed",
        "response": chat_response_json("你好世界"),
        "sequence_number": 6
    });
    let sse_body = format!(
        concat!(
            "event: response.created\n",
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_1\",",
            "\"object\":\"response\",\"created_at\":1,\"model\":\"gpt-test\",",
            "\"capability\":\"chat\",\"status\":\"in_progress\",\"output\":[],",
            "\"usage\":null,\"incomplete_details\":null,\"error\":null}},",
            "\"sequence_number\":0}}\n\n",
            ": keepalive\n\n",
            "event: response.output_text.delta\n",
            "data: {{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",",
            "\"output_index\":0,\"content_index\":0,\"delta\":\"你好\",\"sequence_number\":1}}\n\n",
            "event: response.output_text.delta\n",
            "data: {{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",",
            "\"output_index\":0,\"content_index\":0,\"delta\":\"世界\",\"sequence_number\":2}}\n\n",
            "event: response.output_text.done\n",
            "data: {{\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",",
            "\"output_index\":0,\"content_index\":0,\"text\":\"你好世界\",\"sequence_number\":3}}\n\n",
            "event: response.completed\n",
            "data: {}\n\n",
        ),
        completed_payload
    );

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let request = AiRequest {
        input: Some(AiInput::Messages(vec![AiInputMessage::user_text("hi")])),
        ..Default::default()
    };
    let mut stream = client.response_stream(&request).await.unwrap();

    let mut deltas = String::new();
    let mut saw_created = false;
    let mut saw_done_text = None;
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            AiStreamEvent::Created => saw_created = true,
            AiStreamEvent::OutputTextDelta { delta } => deltas.push_str(&delta),
            AiStreamEvent::OutputTextDone { text } => saw_done_text = Some(text),
            AiStreamEvent::Completed { response } => {
                terminal = Some(response);
            }
            AiStreamEvent::Other { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(saw_created);
    assert_eq!(deltas, "你好世界");
    assert_eq!(saw_done_text.as_deref(), Some("你好世界"));
    let terminal = terminal.expect("terminal event");
    assert_eq!(terminal.status, "completed");
    assert_eq!(terminal.output_text(), "你好世界");
}

#[tokio::test]
async fn test_response_stream_error_event_closes_stream() {
    let (client, mock_server) = create_test_client().await;

    let sse_body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"code\":\"provider_stream_error\",",
        "\"message\":\"AI provider stream failed\",\"param\":null,\"sequence_number\":1}\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let request = AiRequest {
        input: Some(AiInput::Text("hi".into())),
        ..Default::default()
    };
    let mut stream = client.response_stream(&request).await.unwrap();

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }
    assert!(matches!(events.first(), Some(AiStreamEvent::Created)));
    match events.last() {
        Some(AiStreamEvent::Error { code, .. }) => {
            assert_eq!(code, "provider_stream_error");
        }
        other => panic!("expected trailing error event, got {other:?}"),
    }
}

#[tokio::test]
async fn test_response_stream_non_sse_failure_before_stream() {
    let (client, mock_server) = create_test_client().await;

    // 流开始前失败（如 NoChannel 503）：普通 JSON 失败信封而非 SSE
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "id": "resp_f", "object": "response", "created_at": 1,
            "model": null, "capability": null, "status": "failed",
            "incomplete_details": null, "output": [], "usage": null,
            "error": {"code": "service_unavailable",
                       "message": "No active AI channel supports this capability"}
        })))
        .mount(&mock_server)
        .await;

    let request = AiRequest {
        input: Some(AiInput::Text("hi".into())),
        ..Default::default()
    };
    // AiEventStream 不实现 Debug，不能直接 unwrap_err
    let err = match client.response_stream(&request).await {
        Ok(_) => panic!("expected pre-stream failure"),
        Err(err) => err,
    };
    match err {
        ClientError::Api { status, code, .. } => {
            assert_eq!(status, 503);
            assert_eq!(code, "service_unavailable");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn test_unauthorized_envelope_maps_to_api_error() {
    let (client, mock_server) = create_test_client().await;

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "id": "resp_u", "object": "response", "created_at": 1,
            "model": null, "capability": null, "status": "failed",
            "incomplete_details": null, "output": [], "usage": null,
            "error": {"code": "unauthorized", "message": "Missing token"}
        })))
        .mount(&mock_server)
        .await;

    let err = client
        .response(&AiRequest {
            input: Some(AiInput::Text("hi".into())),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ClientError::Api { status: 401, ref code, .. } if code == "unauthorized"
    ));
}

/// 首次 503 失败信封、第二次返回 SSE 成功的响应器（建连阶段重试验证）。
struct FlakyStreamResponder {
    calls: AtomicU32,
    success_body: String,
}

impl Respond for FlakyStreamResponder {
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
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(self.success_body.clone(), "text/event-stream")
        }
    }
}

/// 回归（评审建议测试）：`response_stream` 建连阶段遇 503 时按
/// `max_retries` 退避重试，第二次成功建流并正常消费到终止事件
/// （既有用例均 `max_retries=0`，重试循环未被端到端覆盖）。
#[tokio::test]
async fn test_response_stream_retries_connection_phase_then_succeeds() {
    let mock_server = MockServer::start().await;
    let config = ClientConfig::new(mock_server.uri())
        .with_max_retries(1)
        .with_timeout(10)
        .with_no_proxy(true);
    let client = Client::new(config).expect("valid config");

    let sse_body = format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"sequence_number\":0}}\n\n\
         event: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed", "response": chat_response_json("恢复"), "sequence_number": 1})
    );
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(FlakyStreamResponder {
            calls: AtomicU32::new(0),
            success_body: sse_body,
        })
        .expect(2) // 钉住：恰为一次失败 + 一次重试
        .mount(&mock_server)
        .await;

    let request = AiRequest {
        input: Some(AiInput::Text("hi".into())),
        ..Default::default()
    };
    let mut stream = client
        .response_stream(&request)
        .await
        .expect("second attempt must succeed");
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        if let AiStreamEvent::Completed { response } = event.unwrap() {
            terminal = Some(response);
        }
    }
    let terminal = terminal.expect("terminal event after connection retry");
    assert_eq!(terminal.output_text(), "恢复");
}

/// 回归（非阻断建议 #1 修复）：终止事件载荷非法时流以
/// `Err(Deserialization)` 收敛，而非静默丢弃后伪装成连接断开。
#[tokio::test]
async fn test_response_stream_malformed_terminal_event_yields_error() {
    let (client, mock_server) = create_test_client().await;

    // completed 事件缺 `response` 字段
    let sse_body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":1}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let request = AiRequest {
        input: Some(AiInput::Text("hi".into())),
        ..Default::default()
    };
    let mut stream = client.response_stream(&request).await.unwrap();

    let mut saw_created = false;
    let mut terminal_err = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(AiStreamEvent::Created) => saw_created = true,
            Ok(other) => panic!("unexpected event: {other:?}"),
            Err(err) => terminal_err = Some(err),
        }
    }
    assert!(saw_created);
    assert!(
        matches!(terminal_err, Some(ClientError::Deserialization(_))),
        "malformed terminal event must surface a deserialization error"
    );
}

/// 回归（评审建议测试）：无分隔符的超 4 MiB 事件触发缓冲溢出护栏，
/// 流以恰好一个 `Err(Deserialization)` 收敛而非内存无界增长或挂起。
#[tokio::test]
async fn test_response_stream_buffer_overflow_yields_single_error() {
    let (client, mock_server) = create_test_client().await;

    // 单个“事件”超过 4 MiB 且始终不出现空行分隔符
    let mut body = String::from("event: response.output_text.delta\ndata: \"");
    body.push_str(&"x".repeat(4 * 1024 * 1024 + 1024));
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let request = AiRequest {
        input: Some(AiInput::Text("hi".into())),
        ..Default::default()
    };
    let mut stream = client.response_stream(&request).await.unwrap();

    let mut errors = 0usize;
    while let Some(event) = stream.next().await {
        match event {
            Err(ClientError::Deserialization(message)) => {
                assert!(message.contains("overflow"), "message: {message}");
                errors += 1;
            }
            other => panic!("expected overflow error, got {other:?}"),
        }
    }
    assert_eq!(
        errors, 1,
        "overflow must surface exactly one error then end"
    );
}

/// 回归（评审建议测试）：连接在无终止事件时关闭 —— 流干净结束
/// （既无终止事件也无 Err），“异常结束归上层判定”的契约钉定。
#[tokio::test]
async fn test_response_stream_connection_close_without_terminal_ends_cleanly() {
    let (client, mock_server) = create_test_client().await;

    let sse_body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"半截\",\"sequence_number\":1}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let request = AiRequest {
        input: Some(AiInput::Text("hi".into())),
        ..Default::default()
    };
    let mut stream = client.response_stream(&request).await.unwrap();

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("no error expected on clean close"));
    }
    assert!(matches!(events.first(), Some(AiStreamEvent::Created)));
    assert!(matches!(
        events.last(),
        Some(AiStreamEvent::OutputTextDelta { delta }) if delta == "半截"
    ));
    // 无终止事件：由上层（ModelClient）判定为异常结束并决策重试
    assert!(!events.iter().any(|e| matches!(
        e,
        AiStreamEvent::Completed { .. }
            | AiStreamEvent::Incomplete { .. }
            | AiStreamEvent::Failed { .. }
    )));
}

/// 回归（评审建议测试）：建连重试耗尽后返回**最后一次**的错误
/// （而非丢失为通用 "retries exhausted"），且物理请求次数恰为
/// max_retries + 1。
#[tokio::test]
async fn test_response_stream_retry_exhaustion_returns_last_error() {
    let mock_server = MockServer::start().await;
    let config = ClientConfig::new(mock_server.uri())
        .with_max_retries(1)
        .with_timeout(10)
        .with_no_proxy(true);
    let client = Client::new(config).expect("valid config");

    Mock::given(method("POST"))
        .and(path("/api/ai/response"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "id": "resp_f", "object": "response", "created_at": 1,
            "model": null, "capability": null, "status": "failed",
            "incomplete_details": null, "output": [], "usage": null,
            "error": {"code": "service_unavailable",
                       "message": "AI provider request failed"}
        })))
        .expect(2) // 钉住：首次 + 1 次重试，不多不少
        .mount(&mock_server)
        .await;

    let request = AiRequest {
        input: Some(AiInput::Text("hi".into())),
        ..Default::default()
    };
    let err = match client.response_stream(&request).await {
        Ok(_) => panic!("expected exhaustion failure"),
        Err(err) => err,
    };
    // 最后一次错误的失败信封被保留（非通用 Network 兑底）
    assert!(matches!(
        err,
        ClientError::Api { status: 503, ref code, .. } if code == "service_unavailable"
    ));
}
