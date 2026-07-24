#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for the Responses API (`/api/ai/response`).
//!
//! These tests require running PostgreSQL and Redis instances.
//! Start services before running:
//! - PostgreSQL: default port 5432
//! - Redis: default port 6379
//!
//! Run: cargo test --test axum_responses_test

use ains_axum::{Body, BodyExt, Method, Router, StatusCode};
use serde_json::{Value, json};

mod common;
use common::axum as axum_helpers;

// ── Test helpers ────────────────────────────────────────────────

async fn body_to_json(response: ains_axum::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    parse_body_json(&bytes)
}

fn parse_body_json(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(bytes).expect("non-empty response body must contain valid JSON")
    }
}

async fn post(
    app: &Router,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut headers = vec![("content-type", "application/json")];
    let auth_header;
    if let Some(t) = token {
        auth_header = format!("Bearer {}", t);
        headers.push(("authorization", auth_header.as_str()));
    }
    let headers: Vec<(&str, &str)> = headers;
    let body = body
        .map(|b| serde_json::to_string(b).unwrap())
        .unwrap_or_default();
    let resp = axum_helpers::send_request(app, Method::POST, uri, headers, Body::from(body)).await;
    let status = resp.status();
    let json_body = body_to_json(resp).await;
    (status, json_body)
}

fn error_message(body: &Value) -> String {
    body.get("message")
        .or_else(|| body.get("error").and_then(|error| error.get("message")))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase()
}

#[test]
fn body_json_parser_only_maps_an_empty_body_to_null() {
    assert_eq!(parse_body_json(&[]), Value::Null);
    assert_eq!(parse_body_json(br#"{"ok":true}"#), json!({"ok": true}));
}

#[test]
#[should_panic(expected = "non-empty response body must contain valid JSON")]
fn body_json_parser_does_not_hide_malformed_json() {
    let _ = parse_body_json(b"not-json");
}

#[tokio::test]
async fn test_old_ai_chat_route_is_not_registered() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let (status, _) = post(&app, "/api/ai/chat", None, Some(&json!({"input": "Hello"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_direct_capabilities_reach_unified_route() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let token = axum_helpers::register_isolated_tenant_user(&app, "resp_direct").await;
    let cases = [
        json!({
            "capability": "embedding",
            "model": "embed-model",
            "input": "hello",
            "store": false,
            "tools": []
        }),
        json!({
            "capability": "stt",
            "model": "stt-model",
            "input": {"data": "YWJj", "format": "wav"},
            "store": false,
            "tools": []
        }),
        json!({
            "capability": "tts",
            "model": "tts-model",
            "input": "hello",
            "audio": {"voice": "alloy", "format": "mp3", "speed": 1.0},
            "store": false,
            "tools": []
        }),
    ];

    for body in cases {
        let (status, response) = post(&app, "/api/ai/response", Some(&token), Some(&body)).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "direct capability should pass validation and reach channel selection: {response}"
        );
    }
}

// ── Responses API: Non-streaming ────────────────────────────────

#[tokio::test]
async fn test_responses_text_input_returns_response_format() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let token = axum_helpers::register_isolated_tenant_user(&app, "resp_text").await;

    // Simple text input using Responses API format
    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&token),
        Some(&json!({
            "input": "Hello!"
        })),
    )
    .await;

    // The isolated tenant has no channels, so channel selection returns 503.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_responses_with_instructions() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let token = axum_helpers::register_isolated_tenant_user(&app, "resp_instr").await;

    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&token),
        Some(&json!({
            "input": "What is Rust?",
            "instructions": "You are a helpful Rust expert."
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_responses_with_developer_instructions() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let token = axum_helpers::register_isolated_tenant_user(&app, "resp_dev").await;

    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&token),
        Some(&json!({
            "input": "Hello",
            "instructions": "You are a helpful assistant.",
            "developer_instructions": "Always respond in valid JSON."
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_responses_max_output_tokens() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let token = axum_helpers::register_isolated_tenant_user(&app, "resp_max").await;

    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&token),
        Some(&json!({
            "input": "Hi",
            "max_output_tokens": 2048
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_responses_returns_401_without_auth() {
    let (app, _state) = axum_helpers::create_app_and_state().await;

    let (status, body) = post(
        &app,
        "/api/ai/response",
        None,
        Some(&json!({
            "input": "Hello"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Responses endpoint should require authentication"
    );
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn test_responses_rejects_invalid_format() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("resp_inv");
    let token = axum_helpers::register_and_login(&app, &email).await;

    // Missing 'input' field should be rejected
    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&token),
        Some(&json!({
            "model": "gpt-4o"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing input should be 400"
    );
}

#[tokio::test]
async fn test_responses_multi_turn_messages() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    // Isolated tenant: a sibling test may create an active chat channel in the
    // shared `default` tenant, which this chat request would then select and
    // proxy — surfacing the upstream 4xx instead of the expected NoChannel 503.
    let token = axum_helpers::register_isolated_tenant_user(&app, "resp_mult").await;

    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&token),
        Some(&json!({
            "input": [
                {"role": "user", "content": "What is the capital of France?"},
                {"role": "assistant", "content": "Paris."},
                {"role": "user", "content": "And its population?"}
            ]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_responses_with_image_input() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    // Isolated tenant so no sibling-created channel can satisfy this request and
    // mask the expected NoChannel 503.
    let token = axum_helpers::register_isolated_tenant_user(&app, "resp_img").await;

    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&token),
        Some(&json!({
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "What's in this image?"},
                    {"type": "input_image", "image_url": "https://example.com/photo.jpg"}
                ]
            }]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_responses_web_search_tool() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("resp_web");
    let token = axum_helpers::register_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/ai/response",
        Some(&token),
        Some(&json!({
            "input": "Who is the current president of France?",
            "tools": [{"type": "web_search"}]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_message(&body).contains("tools are not supported"));
}

// ── Responses API: Streaming ────────────────────────────────────

#[tokio::test]
async fn test_responses_streaming_returns_sse() {
    let (app, _state) = axum_helpers::create_app_and_state().await;

    // Isolated tenant with no channels guarantees the 503-without-SSE response.
    let token = axum_helpers::register_isolated_tenant_user(&app, "str_iso").await;

    let uri = "/api/ai/response";
    let body_str = serde_json::to_string(&json!({
        "input": "Hello",
        "stream": true
    }))
    .unwrap();

    let auth_header = format!("Bearer {}", token);
    let mut headers = vec![("content-type", "application/json")];
    headers.push(("authorization", &auth_header));

    let resp =
        axum_helpers::send_request(&app, Method::POST, uri, headers, Body::from(body_str)).await;

    // SSE responses require an active channel; without one it returns 503
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "streaming without a channel should return 503"
    );
    // The error response should be JSON, not SSE
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        !ct.contains("text/event-stream"),
        "without a channel, response should not be SSE, got content-type: {}",
        ct
    );
}

// ── Disabled tenant isolation ───────────────────────────────────

#[tokio::test]
async fn test_responses_disabled_tenant_returns_403() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("resp_dis_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Responses Disabled Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    // Create a user in the new tenant
    let user_email = common::unique_email("resp_dis_user");
    let (status, _body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Disabled Tenant User",
            "tenant_id": tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Login as that user
    let (status, body) = post(
        &app,
        "/api/public/auth/login",
        None,
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_token = body["token"].as_str().unwrap().to_string();

    // Disable the tenant
    let (status, _body) = put_tenant(
        &app,
        &format!("/api/tenants/{}", tenant_id),
        Some(&sys_token),
        Some(&json!({
            "status": "disabled"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // User from disabled tenant should get 403
    let (status, body) = post(
        &app,
        "/api/ai/response",
        Some(&user_token),
        Some(&json!({
            "input": "Hello"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "disabled tenant user should get 403"
    );
    let msg = error_message(&body);
    assert!(
        msg.contains("disabled") || msg.contains("tenant"),
        "error should mention tenant being disabled, got: {}",
        msg
    );
}

// ── Helper for PUT requests ─────────────────────────────────────

async fn put_tenant(
    app: &Router,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut headers = vec![("content-type", "application/json")];
    let auth_header;
    if let Some(t) = token {
        auth_header = format!("Bearer {}", t);
        headers.push(("authorization", auth_header.as_str()));
    }
    let headers: Vec<(&str, &str)> = headers;
    let body = body
        .map(|b| serde_json::to_string(b).unwrap())
        .unwrap_or_default();
    let resp = axum_helpers::send_request(app, Method::PUT, uri, headers, Body::from(body)).await;
    let status = resp.status();
    let json_body = body_to_json(resp).await;
    (status, json_body)
}
