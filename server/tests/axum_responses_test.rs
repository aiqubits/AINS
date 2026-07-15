#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for the Responses API (`/api/ai/chat`).
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
    serde_json::from_slice(&bytes).unwrap()
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
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase()
}

// ── Responses API: Non-streaming ────────────────────────────────

#[tokio::test]
async fn test_responses_text_input_returns_response_format() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("resp_text");
    let token = axum_helpers::register_and_login(&app, &email).await;

    // Simple text input using Responses API format
    let (status, _body) = post(
        &app,
        "/api/ai/chat",
        Some(&token),
        Some(&json!({
            "input": "Hello!"
        })),
    )
    .await;

    // No usable channel exists for this tenant, so should get 503.
    // (Leftover channels from other tests may exist but won't be reachable.)
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_responses_with_instructions() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("resp_instr");
    let token = axum_helpers::register_and_login(&app, &email).await;

    let (status, _body) = post(
        &app,
        "/api/ai/chat",
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
    let email = common::unique_email("resp_dev");
    let token = axum_helpers::register_and_login(&app, &email).await;

    let (status, _body) = post(
        &app,
        "/api/ai/chat",
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
    let email = common::unique_email("resp_max");
    let token = axum_helpers::register_and_login(&app, &email).await;

    let (status, _body) = post(
        &app,
        "/api/ai/chat",
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

    let (status, _body) = post(
        &app,
        "/api/ai/chat",
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
}

#[tokio::test]
async fn test_responses_rejects_invalid_format() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("resp_inv");
    let token = axum_helpers::register_and_login(&app, &email).await;

    // Missing 'input' field should be rejected
    let (status, _body) = post(
        &app,
        "/api/ai/chat",
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
    let email = common::unique_email("resp_mult");
    let token = axum_helpers::register_and_login(&app, &email).await;

    let (status, _body) = post(
        &app,
        "/api/ai/chat",
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
    let email = common::unique_email("resp_img");
    let token = axum_helpers::register_and_login(&app, &email).await;

    let (status, _body) = post(
        &app,
        "/api/ai/chat",
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

    let (status, _body) = post(
        &app,
        "/api/ai/chat",
        Some(&token),
        Some(&json!({
            "input": "Who is the current president of France?",
            "tools": [{"type": "web_search"}]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// ── Responses API: Streaming ────────────────────────────────────

#[tokio::test]
async fn test_responses_streaming_returns_sse() {
    let (app, _state) = axum_helpers::create_app_and_state().await;

    // Create an isolated tenant with no channels to guarantee 503 response
    let sys_email = common::unique_email("str_iso_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;
    let tenant_name = common::unique_table_name("str_tenant");
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": tenant_name,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let isolated_tenant_id = body["id"].as_str().unwrap().to_string();

    // Create a user in the isolated tenant
    let user_email = common::unique_email("str_iso_user");
    let (status, _) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Isolated Streaming User",
            "tenant_id": isolated_tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

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
    let token = body["token"].as_str().unwrap().to_string();

    let uri = "/api/ai/chat";
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
        "/api/ai/chat",
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
