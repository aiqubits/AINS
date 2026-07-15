#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for user balance API endpoints (admin/system only).

use ains_axum::{Body, BodyExt, Method, Router, StatusCode};
use serde_json::{Value, json};

mod common;
use common::axum as axum_helpers;

async fn body_to_json(response: ains_axum::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
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

#[tokio::test]
async fn test_admin_can_set_balance() {
    let app = axum_helpers::create_app().await;
    let admin_email = common::unique_email("set_bal_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let user_email = common::unique_email("set_bal_user");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&admin_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Set Balance User",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    let (status, body) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({
            "balance": 50000000000_i64,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "admin should set balance: {:?}",
        body
    );
    assert_eq!(body["balance"], 50000000000_i64);
    assert!((body["display_balance"].as_f64().unwrap() - 5.0).abs() < 0.001);
    assert!(body["message"].as_str().unwrap().contains("success"));
}

#[tokio::test]
async fn test_system_can_set_balance() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("set_bal_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let user_email = common::unique_email("set_bal_sys_user");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "System Set Balance User",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    let (status, body) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&sys_token),
        Some(&json!({
            "balance": 100000000000_i64,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "system should set balance: {:?}",
        body
    );
    assert_eq!(body["balance"], 100000000000_i64);
}

#[tokio::test]
async fn test_regular_user_cannot_set_balance() {
    let app = axum_helpers::create_app().await;
    let user_email = common::unique_email("set_bal_regular");
    let user_token = axum_helpers::register_and_login(&app, &user_email).await;

    let (status, body) = put(
        &app,
        "/api/users/999999/balance",
        Some(&user_token),
        Some(&json!({
            "balance": 50000,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "regular user cannot set balance: {:?}",
        body
    );
}

#[tokio::test]
async fn test_set_balance_rejects_negative() {
    let app = axum_helpers::create_app().await;
    let admin_email = common::unique_email("set_bal_neg_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let user_email = common::unique_email("set_bal_neg_user");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&admin_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Negative Balance User",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    let (status, body) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({
            "balance": -1,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "negative balance should be rejected: {:?}",
        body
    );
    let msg = error_message(&body);
    assert!(
        msg.contains("negative") || msg.contains("not allowed"),
        "msg: {}",
        msg
    );
}

#[tokio::test]
async fn test_set_balance_on_system_user_returns_404() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("set_bal_sys_on_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, body) = post(
        &app,
        "/api/public/auth/login",
        None,
        Some(&json!({
            "email": sys_email,
            "password": "Password123!",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sys_id: i64 = body["user_id"].as_str().unwrap().parse().unwrap();

    let (status, body) = put(
        &app,
        &format!("/api/users/{}/balance", sys_id),
        Some(&sys_token),
        Some(&json!({
            "balance": 50000,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "should not allow modifying system account: {:?}",
        body
    );
}

#[tokio::test]
async fn test_admin_can_adjust_balance_positive() {
    let app = axum_helpers::create_app().await;
    let admin_email = common::unique_email("adj_bal_pos_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let user_email = common::unique_email("adj_bal_pos_user");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&admin_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Adjust Positive User",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id: i64 = body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(body["balance"], 0, "new user should have 0 balance");

    let (status, body) = post(
        &app,
        &format!("/api/users/{}/balance/adjust", user_id),
        Some(&admin_token),
        Some(&json!({
            "amount": 300000000000_i64,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "admin should adjust balance: {:?}",
        body
    );
    assert_eq!(body["balance"], 300000000000_i64);
    assert!((body["display_balance"].as_f64().unwrap() - 30.0).abs() < 0.001);
}

#[tokio::test]
async fn test_admin_can_adjust_balance_negative() {
    let app = axum_helpers::create_app().await;
    let admin_email = common::unique_email("adj_bal_neg_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let user_email = common::unique_email("adj_bal_neg_user");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&admin_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Adjust Negative User",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    let (status, _body) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({
            "balance": 500000000000_i64,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post(
        &app,
        &format!("/api/users/{}/balance/adjust", user_id),
        Some(&admin_token),
        Some(&json!({
            "amount": -300000000000_i64,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "admin should decrease balance: {:?}",
        body
    );
    assert_eq!(body["balance"], 200000000000_i64);
    assert!((body["display_balance"].as_f64().unwrap() - 20.0).abs() < 0.001);
}

#[tokio::test]
async fn test_adjust_balance_below_zero_rejected() {
    let app = axum_helpers::create_app().await;
    let admin_email = common::unique_email("adj_bal_below_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let user_email = common::unique_email("adj_bal_below_user");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&admin_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Adjust Below Zero User",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    let (status, body) = post(
        &app,
        &format!("/api/users/{}/balance/adjust", user_id),
        Some(&admin_token),
        Some(&json!({
            "amount": -1,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "balance below zero rejected: {:?}",
        body
    );
    let msg = error_message(&body);
    assert!(
        msg.contains("insufficient"),
        "should mention insufficient, got: {}",
        msg
    );
}

#[tokio::test]
async fn test_regular_user_cannot_adjust_balance() {
    let app = axum_helpers::create_app().await;
    let user_email = common::unique_email("adj_bal_regular");
    let user_token = axum_helpers::register_and_login(&app, &user_email).await;

    let (status, body) = post(
        &app,
        "/api/users/999999/balance/adjust",
        Some(&user_token),
        Some(&json!({
            "amount": 100,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "regular user cannot adjust balance: {:?}",
        body
    );
}

// ── Cross-tenant balance isolation tests ─────────────────────────

#[tokio::test]
async fn test_admin_cannot_set_balance_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cross_bal_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Other Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    // Create a user in tenant B
    let user_email = common::unique_email("cross_bal_user_b");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Tenant B User",
            "tenant_id": tenant_b_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // Create an admin in the default tenant
    let admin_email = common::unique_email("cross_bal_admin_a");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    // Admin from default tenant tries to set balance of tenant B's user → NotFound
    let (status, _body) = put(
        &app,
        &format!("/api/users/{}/balance", user_id_b),
        Some(&admin_token),
        Some(&json!({
            "balance": 50000,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "admin should NOT be able to set balance cross-tenant"
    );
}

#[tokio::test]
async fn test_admin_cannot_adjust_balance_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cross_adj_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Adjust Other Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    // Create a user in tenant B with initial balance
    let user_email = common::unique_email("cross_adj_user_b");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Adjust Tenant B User",
            "tenant_id": tenant_b_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // Create an admin in the default tenant
    let admin_email = common::unique_email("cross_adj_admin_a");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    // Admin from default tenant tries to adjust balance of tenant B's user → NotFound
    let (status, _body) = post(
        &app,
        &format!("/api/users/{}/balance/adjust", user_id_b),
        Some(&admin_token),
        Some(&json!({
            "amount": 100000000000_i64,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "admin should NOT be able to adjust balance cross-tenant"
    );
}

#[tokio::test]
async fn test_system_can_set_balance_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cross_bal_sys2");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant with a user
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Cross System Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("cross_bal_sys_user");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Cross Tenant User",
            "tenant_id": tenant_b_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // System CAN set balance of any user regardless of tenant
    let (status, _body) = put(
        &app,
        &format!("/api/users/{}/balance", user_id_b),
        Some(&sys_token),
        Some(&json!({
            "balance": 99999999999_i64,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "system should be able to set balance cross-tenant"
    );
}

#[tokio::test]
async fn test_set_balance_nonexistent_user_returns_404() {
    let app = axum_helpers::create_app().await;
    let admin_email = common::unique_email("set_bal_404_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let (status, body) = put(
        &app,
        "/api/users/999999999999999/balance",
        Some(&admin_token),
        Some(&json!({
            "balance": 50000,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "nonexistent user should 404: {:?}",
        body
    );
}
