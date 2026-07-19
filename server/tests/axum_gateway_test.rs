#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for AI Gateway channel CRUD and proxy dispatch.
//!
//! These tests require running PostgreSQL and Redis instances.
//! Start services before running:
//! - PostgreSQL: default port 5432
//! - Redis: default port 6379
//!
//! Run: cargo test --test axum_gateway_test

use ains_axum::{Body, BodyExt, Method, Router, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

mod common;
use common::axum as axum_helpers;

#[cfg(not(feature = "ains-salvo"))]
use ains_server::utils::config::QuotaConfig;

// ── Test helpers ────────────────────────────────────────────────

async fn body_to_json(response: ains_axum::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut headers = Vec::new();
    if let Some(t) = token {
        headers.push(("authorization", format!("Bearer {}", t)));
    }
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let resp = axum_helpers::send_request(app, Method::GET, uri, headers, Body::empty()).await;
    let status = resp.status();
    let body = body_to_json(resp).await;
    (status, body)
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

async fn put(
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

/// Delete all active channels.
#[allow(dead_code)]
async fn cleanup_channels(app: &Router, sys_token: &str) {
    let (status, body) = get(app, "/api/channels", Some(sys_token)).await;
    if status != StatusCode::OK {
        return;
    }
    // Response is { "items": [...] }
    if let Some(items) = body.get("items").and_then(|v| v.as_array()) {
        for ch in items {
            if let Some(id) = ch["id"].as_str() {
                let _ = delete(app, &format!("/api/channels/{}", id), Some(sys_token)).await;
            }
        }
    }
}

async fn delete(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut headers = Vec::new();
    if let Some(t) = token {
        headers.push(("authorization", format!("Bearer {}", t)));
    }
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let resp = axum_helpers::send_request(app, Method::DELETE, uri, headers, Body::empty()).await;
    let status = resp.status();
    // DELETE returns JSON with message — parse body
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json_body = if bytes.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json_body)
}

/// Extract error message from HttpError JSON response — checks the `message` field.
fn error_message(body: &Value) -> String {
    body.get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase()
}

// ── Channel CRUD Tests ─────────────────────────────────────────

#[tokio::test]
async fn test_channel_create_as_admin() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("ch_create_admin");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&token),
        Some(&json!({
            "name": "Test OpenAI Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat", "embedding"],
            "api_key": "sk-test-key-12345",
            "base_url": "https://api.openai.com",
            "weight": 1,
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["id"].is_string(), "channel should have a UUID id");
    assert_eq!(body["name"], "Test OpenAI Channel");
    assert_eq!(body["protocol_type"], "openai");
    assert_eq!(
        body["is_active"], true,
        "channel should be active by default"
    );
    assert_eq!(body["weight"], 1);
    assert!(
        body.get("api_key_encrypted").is_none(),
        "encrypted API key should not be exposed"
    );
}

#[tokio::test]
async fn test_channel_create_as_admin_sets_own_tenant() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("ch_tenant_admin");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&token),
        Some(&json!({
            "name": "Admin Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-admin-key",
            "base_url": "https://api.openai.com",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant_id"], "default");
}

#[tokio::test]
async fn test_channel_create_system_must_supply_tenant_id() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("ch_sys_create");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, _body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Sys Channel No Tenant",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-sys-key",
            "base_url": "https://api.openai.com",
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "system must supply tenant_id"
    );
}

#[tokio::test]
async fn test_channel_create_system_with_tenant_id() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("ch_sys_tenant");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Sys Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-sys-key",
            "base_url": "https://api.openai.com",
            "tenant_id": "default",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant_id"], "default");
}

#[tokio::test]
async fn test_channel_list_admin_only_sees_own_tenant() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let admin_email_a = common::unique_email("ch_list_a");
    let token_a = axum_helpers::create_admin_and_login(&app, &admin_email_a).await;
    let _ = post(
        &app,
        "/api/channels",
        Some(&token_a),
        Some(&json!({
            "name": "Channel A",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-a",
            "base_url": "https://api.a.com",
        })),
    )
    .await;

    let admin_email_b = common::unique_email("ch_list_b");
    let token_b = axum_helpers::create_admin_and_login(&app, &admin_email_b).await;

    let (status, _body) = post(
        &app,
        "/api/channels",
        Some(&token_b),
        Some(&json!({
            "name": "Channel B",
            "protocol_type": "openai",
            "models": ["claude-3"],
            "capabilities": ["chat"],
            "api_key": "sk-b",
            "base_url": "https://api.b.com",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Admin B lists channels — should only see channels in the default tenant
    let (status, body) = get(&app, "/api/channels", Some(&token_b)).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items should be an array");
    assert!(
        items.len() >= 2,
        "admin should see all channels in their own tenant (>=2), got: {}",
        items.len()
    );
    let names: Vec<&str> = items.iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(
        names.contains(&"Channel B"),
        "Channel B should be in the channel list"
    );
}

#[tokio::test]
async fn test_channel_list_system_sees_all() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("ch_list_all");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;
    let admin_email = common::unique_email("ch_list_all_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let _ = post(
        &app,
        "/api/channels",
        Some(&admin_token),
        Some(&json!({
            "name": "Visible Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-visible",
            "base_url": "https://api.visible.com",
        })),
    )
    .await;

    let (status, body) = get(&app, "/api/channels", Some(&sys_token)).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items should be an array");
    assert!(!items.is_empty(), "system should see all channels");
}

/// 验证分页参数 clamp 后，响应字段（page/per_page/total_pages）与实际数据一致。
///
/// 重点覆盖两个边界：
/// - per_page=200 → 被 clamp 到 100（Service 上限），total_pages 不得因未 clamp 而偏小；
/// - per_page=0   → 被 clamp 到 1，total_pages == total，不得出现 total_pages=0 与实际有数据矛盾。
///
/// 为让 total 确定（不被其它测试在 default 租户创建的渠道污染），在独立租户内测试。
#[tokio::test]
async fn test_channel_list_pagination_clamps_boundaries() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("ch_pg_clamp_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // 隔离租户
    let (status, tbody) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({ "name": "pg-clamp-tenant" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = tbody["id"].as_str().unwrap().to_string();

    // 该租户内的 admin（列出渠道时服务端按自身租户过滤，保证 total 确定）
    let admin_email = common::unique_email("ch_pg_clamp_admin");
    let (status, _b) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": admin_email,
            "password": "Password123!",
            "name": "pg clamp admin",
            "role": "admin",
            "tenant_id": tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, lb) = post(
        &app,
        "/api/public/auth/login",
        None,
        Some(&json!({ "email": admin_email, "password": "Password123!" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let admin_token = lb["token"].as_str().unwrap().to_string();

    // 在隔离租户内创建 3 个渠道
    for i in 0..3 {
        let (status, _b) = post(
            &app,
            "/api/channels",
            Some(&admin_token),
            Some(&json!({
                "name": format!("pg-clamp-ch-{i}"),
                "protocol_type": "openai",
                "models": ["gpt-4"],
                "capabilities": ["chat"],
                "api_key": format!("sk-pg-{i}"),
                "base_url": "https://api.pg.com",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    // 边界 1：per_page=200 → clamp 到 100
    let (status, body) = get(
        &app,
        "/api/channels?page=1&per_page=200",
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page"], 1);
    assert_eq!(
        body["per_page"], 100,
        "per_page 应被 clamp 到 100，而非回传 200"
    );
    assert_eq!(body["total"], 3);
    assert_eq!(body["total_pages"], 1, "ceil(3/100) = 1");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "3 条均在一页内返回");

    // 边界 2：per_page=0 → clamp 到 1
    let (status, body) = get(&app, "/api/channels?page=1&per_page=0", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page"], 1);
    assert_eq!(body["per_page"], 1, "per_page 应被 clamp 到 1，而非回传 0");
    assert_eq!(body["total"], 3);
    assert_eq!(
        body["total_pages"], 3,
        "ceil(3/1) = 3，不得出现 total_pages=0 与实际有数据的矛盾"
    );
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "clamp 后每页仅 1 条");
}

#[tokio::test]
async fn test_channel_update() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("ch_update");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&token),
        Some(&json!({
            "name": "Original Name",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-original",
            "base_url": "https://api.original.com",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let channel_id = body["id"].as_str().unwrap();

    let (status, body) = put(
        &app,
        &format!("/api/channels/{}", channel_id),
        Some(&token),
        Some(&json!({
            "name": "Updated Name",
            "weight": 5,
            "is_active": false,
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Updated Name");
    assert_eq!(body["weight"], 5);
    assert_eq!(body["is_active"], false);
}

#[tokio::test]
async fn test_channel_disable_soft_delete() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("ch_disable");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&token),
        Some(&json!({
            "name": "To Disable",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-disable",
            "base_url": "https://api.disable.com",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let channel_id = body["id"].as_str().unwrap().to_string();

    // Soft-disable via PUT is_active=false
    let (status, _body) = put(
        &app,
        &format!("/api/channels/{}", channel_id),
        Some(&token),
        Some(&json!({"is_active": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify it's disabled by fetching
    let (status, body) = get(&app, "/api/channels", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let channel = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == channel_id)
        .expect("channel should still exist after soft disable");
    assert_eq!(
        channel["is_active"], false,
        "channel should be disabled, not deleted"
    );
    assert_eq!(
        channel["name"], "To Disable",
        "channel metadata should be preserved"
    );
}

/// Hard-delete must be refused (409 Conflict) when the channel still has
/// associated `token_usage` records, so historical accounting rows are never
/// orphaned. The guard reads the usage count from the write connection, so a
/// just-recorded usage row is always visible here.
#[tokio::test]
async fn test_channel_delete_rejected_when_usage_exists() {
    let (app, state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("ch_del_usage");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    // Create a channel to delete.
    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&token),
        Some(&json!({
            "name": "Has Usage",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-has-usage",
            "base_url": "https://api.hasusage.com",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let channel_id = body["id"].as_str().unwrap().to_string();
    let channel_uuid: uuid::Uuid = channel_id.parse().unwrap();
    let tenant_id = body["tenant_id"].as_str().unwrap().to_string();

    // Record a token_usage row referencing this channel.
    let metering = ains_server::services::MeteringService::new(state.db.clone());
    metering
        .record_usage(
            1_i64,
            &tenant_id,
            channel_uuid,
            "gpt-4",
            "chat",
            &json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}),
        )
        .await
        .expect("record usage should succeed");

    // DELETE must be refused with 409 Conflict.
    let (status, del_body) =
        delete(&app, &format!("/api/channels/{}", channel_id), Some(&token)).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "deleting a channel with usage must return 409, got {status}: {del_body:?}"
    );
    assert!(
        error_message(&del_body).contains("usage"),
        "409 message should mention usage records: {del_body:?}"
    );

    // Channel must still exist (not deleted).
    let (status, list_body) = get(&app, "/api/channels", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list_body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == channel_id),
        "channel must survive a rejected delete"
    );
}

#[tokio::test]
async fn test_channel_create_invalid_input_rejected() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("ch_invalid");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    let (status, _body) = post(
        &app,
        "/api/channels",
        Some(&token),
        Some(&json!({
            "name": "Bad Channel",
            "protocol_type": "openai",
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing fields should be rejected"
    );
}

// ── AI Proxy Dispatch Tests (Responses API format) ────────────

#[tokio::test]
async fn test_responses_chat_returns_503_when_no_channel() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("ai_chat_noch");
    let token = axum_helpers::register_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/ai/chat",
        Some(&token),
        Some(&json!({
            "input": "hello"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let err = body
        .get("error")
        .or(body.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        err.to_lowercase().contains("no active") || err.to_lowercase().contains("unavailable"),
        "should indicate no channel available, got: {}",
        err
    );
}

#[tokio::test]
async fn test_responses_chat_returns_401_without_auth() {
    let (app, _state) = axum_helpers::create_app_and_state().await;

    let (status, _body) = post(
        &app,
        "/api/ai/chat",
        None,
        Some(&json!({
            "input": "hello"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "AI endpoints should require authentication"
    );
}

#[tokio::test]
async fn test_channel_minimal_weight_defaults() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("ch_minimal");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&token),
        Some(&json!({
            "name": "Minimal Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-minimal",
            "base_url": "https://api.minimal.com",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["is_active"], true, "is_active should default to true");
    assert_eq!(body["weight"], 1, "weight should default to 1");
}

// ── Disabled tenant isolation tests ─────────────────────────────

#[tokio::test]
async fn test_disabled_tenant_ai_endpoint_returns_403() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("di_tenant_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "To be disabled"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    // Create a user in the new tenant
    let user_email = common::unique_email("di_tenant_user");
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

    // Login as that user to get a token
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
    let (status, _body) = put(
        &app,
        &format!("/api/tenants/{}", tenant_id),
        Some(&sys_token),
        Some(&json!({
            "status": "disabled"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // User from disabled tenant should get 403 on AI endpoint
    let (status, body) = post(
        &app,
        "/api/ai/chat",
        Some(&user_token),
        Some(&json!({
            "input": "hello"
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

#[tokio::test]
async fn test_disabled_tenant_user_cannot_login() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("di_login_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a tenant and a user in it
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Login Test Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("di_login_user");
    let (status, _body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Login Test User",
            "tenant_id": tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Disable the tenant
    let (status, _body) = put(
        &app,
        &format!("/api/tenants/{}", tenant_id),
        Some(&sys_token),
        Some(&json!({
            "status": "disabled"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // User should no longer be able to login
    let (status, _body) = post(
        &app,
        "/api/public/auth/login",
        None,
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "disabled tenant user should not be able to login"
    );
}

// ── Cross-tenant channel isolation tests ────────────────────────

#[tokio::test]
async fn test_cross_tenant_channel_update_isolation() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("cross_ch_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Isolation Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    // Create an admin user in the new tenant (via system API)
    let admin_b_email = common::unique_email("cross_admin_b");
    let (status, _body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": admin_b_email,
            "password": "Password123!",
            "name": "Admin of Tenant B",
            "role": "admin",
            "tenant_id": tenant_b_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Login as admin B
    let (status, body) = post(
        &app,
        "/api/public/auth/login",
        None,
        Some(&json!({
            "email": admin_b_email,
            "password": "Password123!",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let admin_b_token = body["token"].as_str().unwrap().to_string();

    // System creates a channel in default tenant
    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Default Tenant Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-default",
            "base_url": "https://api.default.com",
            "tenant_id": "default",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let default_channel_id = body["id"].as_str().unwrap().to_string();

    // Admin B tries to update default tenant's channel — should get 404
    let (status, _body) = put(
        &app,
        &format!("/api/channels/{}", default_channel_id),
        Some(&admin_b_token),
        Some(&json!({
            "name": "Hacked Channel"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "admin from tenant B should not be able to update default tenant's channel"
    );
}

#[tokio::test]
async fn test_cross_tenant_channel_disable_isolation() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("cross_dis_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant with its own admin and channel
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Disable Isolation Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    // System creates a channel in the default tenant
    let (status, _body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Other Tenant Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-other",
            "base_url": "https://api.other.com",
            "tenant_id": "default",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Login as default tenant admin
    let admin_default_email = common::unique_email("cross_dis_admin");
    let admin_default_token =
        axum_helpers::create_admin_and_login(&app, &admin_default_email).await;

    // Create a channel in tenant B via system
    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Tenant B Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-tenB",
            "base_url": "https://api.tenb.com",
            "tenant_id": tenant_b_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_channel_id = body["id"].as_str().unwrap().to_string();

    // Default admin cannot delete tenant B's channel
    let (status, _body) = delete(
        &app,
        &format!("/api/channels/{}", tenant_b_channel_id),
        Some(&admin_default_token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "default admin should not be able to delete tenant B's channel"
    );

    // Tenant B admin can delete their own channel
    let admin_b_email = common::unique_email("cross_dis_admin_b");
    let (status, _body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": admin_b_email,
            "password": "Password123!",
            "name": "Admin B",
            "role": "admin",
            "tenant_id": tenant_b_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(
        &app,
        "/api/public/auth/login",
        None,
        Some(&json!({
            "email": admin_b_email,
            "password": "Password123!",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let admin_b_token = body["token"].as_str().unwrap().to_string();

    let (status, _body) = delete(
        &app,
        &format!("/api/channels/{}", tenant_b_channel_id),
        Some(&admin_b_token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "tenant B admin should be able to delete their own channel"
    );
}

// ── End-to-end proxy with mock upstream ─────────────────────────

/// Start a mock HTTP server that returns an OpenAI-compatible response.
/// Returns the port the server is listening on.
async fn start_mock_upstream() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = vec![0; 4096];
                let _n = sock.read(&mut buf).await.ok();

                let body = r#"{"id":"chatcmpl-e2e","object":"chat.completion","model":"gpt-4","choices":[{"index":0,"message":{"role":"assistant","content":"Mock upstream response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":10,"total_tokens":30}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });

    port
}

/// Start a mock HTTP server that returns an error for testing failure paths.
async fn start_mock_upstream_error() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = vec![0; 4096];
                let _n = sock.read(&mut buf).await.ok();

                let body =
                    r#"{"error":{"message":"Rate limit exceeded","type":"rate_limit_error"}}"#;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });

    port
}

#[tokio::test]
async fn test_responses_e2e_with_mock_upstream() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let port = start_mock_upstream().await;

    // Create an isolated tenant for this test
    let sys_email = common::unique_email("e2e_iso_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;
    let tenant_name = common::unique_table_name("e2e_tenant");
    let (status, tenant_resp) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": tenant_name,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let isolated_tenant_id = tenant_resp["id"].as_str().unwrap().to_string();

    // Create a chat channel in the isolated tenant pointing to the mock server
    let base_url = format!("http://127.0.0.1:{}", port);
    let channel_body = json!({
        "name": "E2E Mock Channel",
        "protocol_type": "openai",
        "models": ["gpt-4"],
        "capabilities": ["chat"],
        "api_key": "sk-mock",
        "base_url": base_url,
        "tenant_id": isolated_tenant_id,
        "weight": 10_000,
    });
    let (status, _channel_resp) =
        post(&app, "/api/channels", Some(&sys_token), Some(&channel_body)).await;
    assert_eq!(status, StatusCode::OK, "channel creation failed");

    // Create a user in the isolated tenant
    let user_email = common::unique_email("e2e_iso_user");
    let (status, _user_body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Isolated E2E User",
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
    let user_token = body["token"].as_str().unwrap().to_string();

    // Call chat endpoint with Responses API format
    let (status, body) = post(
        &app,
        "/api/ai/chat",
        Some(&user_token),
        Some(&json!({
            "input": "hello"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "chat endpoint should return 200 with mock upstream"
    );

    // Verify Responses API response format
    assert!(
        body["id"].as_str().unwrap_or("").starts_with("resp_"),
        "response id should start with 'resp_', got: {:?}",
        body["id"]
    );
    assert_eq!(body["object"], "response", "object should be 'response'");

    // Check the output structure
    let output = body["output"]
        .as_array()
        .expect("output should be an array");
    assert!(!output.is_empty(), "output should have at least one item");
    assert_eq!(
        output[0]["type"], "message",
        "first output item should be a message"
    );
    let content = output[0]["content"]
        .as_array()
        .expect("content should be an array");
    assert!(!content.is_empty(), "content should have at least one item");
    assert_eq!(
        content[0]["text"], "Mock upstream response",
        "response text should come from mock upstream"
    );

    // Verify usage data is present
    assert_eq!(
        body["usage"]["total_tokens"], 30,
        "usage data should be proxied"
    );
}

#[tokio::test]
async fn test_responses_upstream_error_is_proxied() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let port = start_mock_upstream_error().await;

    // Create an isolated tenant for this test
    let sys_email = common::unique_email("err_iso_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;
    let tenant_name = common::unique_table_name("err_tenant");
    let (status, tenant_resp) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": tenant_name,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let isolated_tenant_id = tenant_resp["id"].as_str().unwrap().to_string();

    // Create a chat channel pointing to the error mock in the isolated tenant
    let base_url = format!("http://127.0.0.1:{}", port);
    let channel_body = json!({
        "name": "E2E Error Channel",
        "protocol_type": "openai",
        "models": ["gpt-4"],
        "capabilities": ["chat"],
        "api_key": "sk-mock-err",
        "base_url": base_url,
        "tenant_id": isolated_tenant_id,
        "weight": 10_000,
    });
    let (status, _) = post(&app, "/api/channels", Some(&sys_token), Some(&channel_body)).await;
    assert_eq!(status, StatusCode::OK);

    // Create a user in the isolated tenant
    let user_email = common::unique_email("err_iso_user");
    let (status, _) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Isolated Error User",
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
    let user_token = body["token"].as_str().unwrap().to_string();

    let (status, body) = post(
        &app,
        "/api/ai/chat",
        Some(&user_token),
        Some(&json!({
            "input": "hello"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "upstream 429 should be mapped to 503"
    );
    let msg = error_message(&body);
    assert!(
        msg.contains("upstream") || msg.contains("provider"),
        "error should mention upstream failure, got: {}",
        msg
    );
}

#[tokio::test]
async fn test_responses_capability_routing_only_matching_capability() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("cap_routing_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create an isolated tenant for this test (avoid DB pollution from E2E tests)
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Capability Routing Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let isolated_tenant_id = body["id"].as_str().unwrap().to_string();

    // Create a channel that only supports "embedding" in the isolated tenant
    let (status, _body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Embedding Only",
            "protocol_type": "openai",
            "models": ["text-embedding-3-small"],
            "capabilities": ["embedding"],
            "api_key": "sk-embed-only",
            "base_url": "https://api.embedding.com",
            "tenant_id": isolated_tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Create a user in the isolated tenant
    let user_email = common::unique_email("cap_routing_user");
    let (status, _body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Isolated User",
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
    let user_token = body["token"].as_str().unwrap().to_string();

    // Chat endpoint should get "no channel" (embedding channel not matched)
    let (status, body) = post(
        &app,
        "/api/ai/chat",
        Some(&user_token),
        Some(&json!({
            "input": "hello"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let msg = error_message(&body);
    assert!(
        msg.contains("no active"),
        "chat should get 'no channel' since only embedding channel exists, got: {}",
        msg
    );
}

#[tokio::test]
async fn test_responses_tenant_isolation_in_routing() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("iso_routing_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create tenant B with a chat channel
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Routing Isolation Tenant B"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    let (status, _body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Tenant B Chat",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-tenantB-chat",
            "base_url": "https://api.tenantb.com",
            "tenant_id": tenant_b_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Create tenant C with NO channels at all (isolated)
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Routing Isolation Tenant C"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_c_id = body["id"].as_str().unwrap().to_string();

    // Create a user in tenant C and login
    let user_email = common::unique_email("iso_routing_user");
    let (status, _body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Tenant C User",
            "tenant_id": tenant_c_id,
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
    let user_token = body["token"].as_str().unwrap().to_string();

    // Tenant C's user should NOT see tenant B's chat channel
    let (status, body) = post(
        &app,
        "/api/ai/chat",
        Some(&user_token),
        Some(&json!({
            "input": "hello"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let msg = error_message(&body);
    assert!(
        msg.contains("no active"),
        "tenant C user should NOT see tenant B's channel, got: {}",
        msg
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Anthropic E2E proxy tests (Responses API format)
// ═══════════════════════════════════════════════════════════════════

/// Start a mock HTTP server that returns an Anthropic-compatible response.
/// Returns the port the server is listening on.
async fn start_mock_anthropic_upstream() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = vec![0; 4096];
                let _n = sock.read(&mut buf).await.ok();

                let body = r#"{"id":"msg_mock_e2e_anthropic","type":"message","role":"assistant","content":[{"type":"text","text":"Mock Anthropic response"}],"model":"claude-3-opus-20240229","usage":{"input_tokens":15,"output_tokens":25}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });

    port
}

/// Start a mock HTTP server that returns an Anthropic error response.
async fn start_mock_anthropic_upstream_error() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = vec![0; 4096];
                let _n = sock.read(&mut buf).await.ok();

                let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limit exceeded"}}"#;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });

    port
}

#[tokio::test]
async fn test_anthropic_responses_e2e_with_mock_upstream() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let port = start_mock_anthropic_upstream().await;

    // Create an isolated tenant for this test
    let sys_email = common::unique_email("anth_iso_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;
    let tenant_name = common::unique_table_name("anth_tenant");
    let (status, tenant_resp) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": tenant_name,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let isolated_tenant_id = tenant_resp["id"].as_str().unwrap().to_string();

    // Create an Anthropic chat channel in the isolated tenant pointing to the mock server
    let base_url = format!("http://127.0.0.1:{}", port);
    let channel_body = json!({
        "name": "Anthropic E2E Channel",
        "protocol_type": "anthropic",
        "models": ["claude-3-opus-20240229"],
        "capabilities": ["chat"],
        "api_key": "sk-ant-mock",
        "base_url": base_url,
        "tenant_id": isolated_tenant_id,
        "weight": 10_000,
    });
    let (status, _) = post(&app, "/api/channels", Some(&sys_token), Some(&channel_body)).await;
    assert_eq!(status, StatusCode::OK, "Anthropic channel creation failed");

    // Create a user in the isolated tenant
    let user_email = common::unique_email("anth_iso_user");
    let (status, _) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Isolated Anthropic User",
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
    let user_token = body["token"].as_str().unwrap().to_string();

    // Call chat endpoint with Responses API format
    let (status, body) = post(
        &app,
        "/api/ai/chat",
        Some(&user_token),
        Some(&json!({
            "input": "Hello, Claude!"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "Anthropic chat endpoint should return 200 with mock upstream"
    );

    // Should get Responses API format response
    assert!(
        body["id"].as_str().unwrap_or("").starts_with("resp_"),
        "response id should start with 'resp_'"
    );
    assert_eq!(body["object"], "response");

    let output = body["output"]
        .as_array()
        .expect("output should be an array");
    assert!(!output.is_empty());
    let content = output[0]["content"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|c| c["text"].as_str())
        .unwrap_or("");
    assert_eq!(
        content, "Mock Anthropic response",
        "response content should be proxied"
    );
}

#[tokio::test]
async fn test_anthropic_responses_upstream_error_is_proxied() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let port = start_mock_anthropic_upstream_error().await;

    // Create an isolated tenant for this test
    let sys_email = common::unique_email("anth_err_iso_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;
    let tenant_name = common::unique_table_name("anth_err_tenant");
    let (status, tenant_resp) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": tenant_name,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let isolated_tenant_id = tenant_resp["id"].as_str().unwrap().to_string();

    // Create an Anthropic chat channel in the isolated tenant pointing to the error mock
    let base_url = format!("http://127.0.0.1:{}", port);
    let channel_body = json!({
        "name": "Anthropic Error Channel",
        "protocol_type": "anthropic",
        "models": ["claude-3-opus-20240229"],
        "capabilities": ["chat"],
        "api_key": "sk-ant-err",
        "base_url": base_url,
        "tenant_id": isolated_tenant_id,
        "weight": 10_000,
    });
    let (status, _) = post(&app, "/api/channels", Some(&sys_token), Some(&channel_body)).await;
    assert_eq!(status, StatusCode::OK);

    // Create a user in the isolated tenant
    let user_email = common::unique_email("anth_err_iso_user");
    let (status, _) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Isolated Anthropic Error User",
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
    let user_token = body["token"].as_str().unwrap().to_string();

    let (status, body) = post(
        &app,
        "/api/ai/chat",
        Some(&user_token),
        Some(&json!({
            "input": "Hello"
        })),
    )
    .await;

    // Anthropic upstream 429 should be mapped to 503 as well
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "Anthropic upstream error should be mapped to 503"
    );
    let msg = error_message(&body);
    assert!(
        msg.contains("upstream") || msg.contains("provider"),
        "error should mention upstream failure, got: {}",
        msg
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Metering skip on upstream error
// ═══════════════════════════════════════════════════════════════════

/// When the upstream returns an error, the proxy must NOT record token
/// metering (no rows in token_usage for the failed request).
#[tokio::test]
async fn test_metering_not_recorded_on_upstream_error() {
    use ains_server::repositories::token_usage::Column as UsageColumn;
    use ains_server::repositories::token_usage::Entity as UsageEntity;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let port = start_mock_upstream_error().await;
    let config = QuotaConfig {
        channel_max_rpm: 100,
        channel_max_tpm: 1_000_000,
        tenant_max_rpm: 100,
        ..Default::default()
    };
    let app = axum_helpers::create_app_with_quota(config).await;

    // Create a system user, a tenant, and a channel pointing to the error mock
    let sys_email = common::unique_email("met_err_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let tenant_name = common::unique_table_name("met_err_t");
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({"name": tenant_name})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    let base_url = format!("http://127.0.0.1:{}", port);
    let (status, _body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Error Channel for Metering",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-meter-err",
            "base_url": base_url,
            "tenant_id": tenant_id,
            "weight": 10_000,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "channel creation failed");

    // Create a user in the tenant and login
    let user_email = common::unique_email("met_err_user");
    let (status, _body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Metering Error User",
            "tenant_id": tenant_id,
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
    let user_token = body["token"].as_str().unwrap().to_string();
    let user_id = body["user_id"].as_str().unwrap().to_string();
    let uid: i64 = user_id.parse().unwrap();

    // Send a chat request — should get 503
    let (status, _body) = post(
        &app,
        "/api/ai/chat",
        Some(&user_token),
        Some(&json!({"input": "hello"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "upstream error should be mapped to 503"
    );

    // Verify NO metering record was created for this user
    let db = common::create_test_db_and_run_migrations().await;
    let records = UsageEntity::find()
        .filter(UsageColumn::UserId.eq(uid))
        .all(&*db)
        .await
        .expect("failed to query token_usage");
    assert!(
        records.is_empty(),
        "no token_usage records should exist for user {} after upstream error, got: {}",
        uid,
        records.len()
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Channel DELETE (hard delete) tests
// ═══════════════════════════════════════════════════════════════════

/// System 可以删除无用量记录的渠道 → 200
#[tokio::test]
async fn test_delete_channel_by_system_ok() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("del_ch_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // 创建渠道
    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Delete Test Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-delete-test",
            "base_url": "https://api.example.com",
            "tenant_id": "default",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let channel_id = body["id"].as_str().unwrap().to_string();

    // 确认渠道存在
    let (status, body) = get(&app, "/api/channels", Some(&sys_token)).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert!(ids.contains(&channel_id.as_str()));

    // 执行物理删除 → 200
    let (status, body) = delete(
        &app,
        &format!("/api/channels/{}", channel_id),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["message"], "Channel deleted successfully");

    // 确认渠道已从列表消失
    let (status, body) = get(&app, "/api/channels", Some(&sys_token)).await;
    assert_eq!(status, StatusCode::OK);
    let ids_after: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert!(!ids_after.contains(&channel_id.as_str()));
}

/// Admin 可以删除本租户下无用量记录的渠道 → 200
#[tokio::test]
async fn test_delete_channel_by_admin_ok() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let admin_email = common::unique_email("del_ch_adm");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&admin_token),
        Some(&json!({
            "name": "Admin Delete Test",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-admin-delete",
            "base_url": "https://api.admin-test.com",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let channel_id = body["id"].as_str().unwrap().to_string();

    let (status, _) = delete(
        &app,
        &format!("/api/channels/{}", channel_id),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// 删除不存在的渠道 → 404
#[tokio::test]
async fn test_delete_channel_not_found() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("del_ch_nf");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let fake_id = "00000000-0000-0000-0000-000000000000";
    let (status, _) = delete(
        &app,
        &format!("/api/channels/{}", fake_id),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Admin 尝试删除其他租户的渠道 → 404（跨租户隔离）
#[tokio::test]
async fn test_delete_channel_cross_tenant_forbidden() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("del_ch_ct_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // System 在另一个租户下创建渠道
    let tenant_name = common::unique_table_name("del_ct_t");
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({"name": tenant_name})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let other_tenant_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Cross-tenant Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-cross",
            "base_url": "https://api.other-tenant.com",
            "tenant_id": other_tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let channel_id = body["id"].as_str().unwrap().to_string();

    // Admin 来自 default 租户，尝试删除 → 404（频道不可见）
    let admin_email = common::unique_email("del_ch_ct_adm");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let (status, _) = delete(
        &app,
        &format!("/api/channels/{}", channel_id),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// 渠道存在关联的 token_usage 记录时删除 → 409
#[tokio::test]
async fn test_delete_channel_with_usage_conflict() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("del_ch_use_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // 创建渠道
    let (status, body) = post(
        &app,
        "/api/channels",
        Some(&sys_token),
        Some(&json!({
            "name": "Channel With Usage",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-usage-test",
            "base_url": "https://api.usage-test.com",
            "tenant_id": "default",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let channel_id = body["id"].as_str().unwrap().to_string();
    let channel_uuid: uuid::Uuid = channel_id.parse().unwrap();

    // 直接插入一条 token_usage 记录到数据库
    let db = common::create_test_db_and_run_migrations().await;
    let db_conn = db.write_conn();
    let now = chrono::Utc::now();
    sea_orm::EntityTrait::insert(ains_server::repositories::token_usage::ActiveModel {
        id: sea_orm::Set(ains_server::snowflake::generate_id()),
        user_id: sea_orm::Set(1),
        tenant_id: sea_orm::Set("default".to_string()),
        channel_id: sea_orm::Set(channel_uuid),
        model: sea_orm::Set("gpt-4".to_string()),
        prompt_tokens: sea_orm::Set(10),
        completion_tokens: sea_orm::Set(20),
        total_tokens: sea_orm::Set(30),
        request_type: sea_orm::Set("chat".to_string()),
        created_at: sea_orm::Set(now),
    })
    .exec(db_conn)
    .await
    .expect("failed to insert token_usage record");

    // 尝试删除 → 409 Conflict
    let (status, body) = delete(
        &app,
        &format!("/api/channels/{}", channel_id),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("token usage"),
        "expected conflict message about token usage, got: {:?}",
        body
    );
}
