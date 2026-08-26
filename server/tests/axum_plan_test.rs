#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for plan management, assignment, purchase and the
//! /api/ai/response plan-quota gate.

use ains_axum::{Body, BodyExt, Method, Router, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

mod common;
use common::axum as axum_helpers;

/// Start a mock upstream that returns an OpenAI-compatible chat completion.
/// Mirrors `start_mock_upstream` in axum_gateway_test.rs; consumption tests
/// need a real (mock) channel because NoChannel failures refund the call.
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

                let body = r#"{"id":"chatcmpl-plan","object":"chat.completion","model":"gpt-4","choices":[{"index":0,"message":{"role":"assistant","content":"Mock upstream response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":10,"total_tokens":30}}"#;
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

/// Create a chat channel in `tenant_id` pointing at the mock upstream.
async fn create_mock_channel(app: &Router, sys_token: &str, tenant_id: &str, port: u16) {
    let (status, body) = post(
        app,
        "/api/channels",
        Some(sys_token),
        Some(&json!({
            "name": "Plan Test Channel",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-plan-mock",
            "base_url": format!("http://127.0.0.1:{}", port),
            "tenant_id": tenant_id,
            "weight": 10_000,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "mock channel creation failed: {body}"
    );
}

async fn body_to_json(response: ains_axum::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn request(
    app: &Router,
    method: Method,
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
    let body = body
        .map(|b| serde_json::to_string(b).unwrap())
        .unwrap_or_default();
    let resp = axum_helpers::send_request(app, method, uri, headers, Body::from(body)).await;
    let status = resp.status();
    (status, body_to_json(resp).await)
}

async fn get(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    request(app, Method::GET, uri, token, None).await
}
async fn post(
    app: &Router,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    request(app, Method::POST, uri, token, body).await
}
async fn put(
    app: &Router,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    request(app, Method::PUT, uri, token, body).await
}
async fn delete(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    request(app, Method::DELETE, uri, token, None).await
}

/// Create a plan via API and return its ID.
async fn create_plan(
    app: &Router,
    token: &str,
    tenant_id: Option<&str>,
    name: &str,
    price: i64,
    total_calls: i64,
    validity_days: i32,
) -> String {
    create_plan_with_limit(
        app,
        token,
        tenant_id,
        name,
        price,
        total_calls,
        validity_days,
        None,
    )
    .await
}

/// Create a plan with an optional per-user cumulative purchase limit.
#[allow(clippy::too_many_arguments)]
async fn create_plan_with_limit(
    app: &Router,
    token: &str,
    tenant_id: Option<&str>,
    name: &str,
    price: i64,
    total_calls: i64,
    validity_days: i32,
    purchase_limit: Option<i32>,
) -> String {
    let mut body = json!({
        "name": name,
        "price": price,
        "total_calls": total_calls,
        "validity_days": validity_days,
    });
    if let Some(t) = tenant_id {
        body["tenant_id"] = json!(t);
    }
    if let Some(limit) = purchase_limit {
        body["purchase_limit"] = json!(limit);
    }
    let (status, resp) = post(app, "/api/plans", Some(token), Some(&body)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "plan creation should succeed: {resp}"
    );
    assert_eq!(
        resp["purchase_limit"],
        purchase_limit.map_or(Value::Null, Value::from),
        "purchase-limit response contract must match create input: {resp}"
    );
    resp["id"].as_str().unwrap().to_string()
}

/// Create a tenant via API (system token) and return its ID.
async fn create_tenant(app: &Router, sys_token: &str, name: &str) -> String {
    let (status, resp) = post(
        app,
        "/api/tenants",
        Some(sys_token),
        Some(&json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tenant creation should succeed");
    resp["id"].as_str().unwrap().to_string()
}

/// Create a user via API and return its ID (as string).
async fn create_user(app: &Router, token: &str, email: &str, tenant_id: Option<&str>) -> String {
    let mut body = json!({
        "email": email,
        "password": common::DEFAULT_TEST_PASSWORD,
        "name": "Plan Test User",
    });
    if let Some(t) = tenant_id {
        body["tenant_id"] = json!(t);
    }
    let (status, resp) = post(app, "/api/users", Some(token), Some(&body)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "user creation should succeed: {resp}"
    );
    resp["id"].as_str().unwrap().to_string()
}

// ── CRUD & permissions ─────────────────────────────────────────

#[tokio::test]
async fn test_plan_crud_as_admin() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("plan_crud_adm")).await;

    // Create (admin is forced to own tenant — no tenant_id needed)
    let plan_name = format!("CRUD Plan {}", common::unique_table_name("p"));
    let plan_id = create_plan(
        &app,
        &admin_token,
        None,
        &plan_name,
        10_000_000_000,
        100,
        30,
    )
    .await;

    // List — the created plan must be present (default tenant is shared,
    // so only assert containment).
    let (status, body) = get(&app, "/api/plans?page=1&per_page=100", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);
    let found = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == plan_id.as_str());
    assert!(found, "created plan should appear in the listing");

    // Update price and status
    let (status, body) = put(
        &app,
        &format!("/api/plans/{}", plan_id),
        Some(&admin_token),
        Some(&json!({ "price": 20_000_000_000i64, "status": "disabled" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "plan update should succeed: {body}");
    assert_eq!(body["price"], 20_000_000_000i64);
    assert_eq!(body["status"], "disabled");

    // Delete
    let (status, _) = delete(&app, &format!("/api/plans/{}", plan_id), Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);

    // Update after delete → 404
    let (status, _) = put(
        &app,
        &format!("/api/plans/{}", plan_id),
        Some(&admin_token),
        Some(&json!({ "price": 1i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_plan_management_requires_admin() {
    let app = axum_helpers::create_app().await;
    let user_token =
        axum_helpers::register_and_login(&app, &common::unique_email("plan_perm_user")).await;

    let (status, _) = get(&app, "/api/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "user cannot list plans");

    let (status, _) = post(
        &app,
        "/api/plans",
        Some(&user_token),
        Some(&json!({ "name": "x", "price": 1, "total_calls": 1, "validity_days": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "user cannot create plans");

    let (status, _) = get(&app, "/api/orders", Some(&user_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "user cannot list orders");
}

#[tokio::test]
async fn test_plan_price_allows_zero_on_create_and_update() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("free_plan_adm")).await;

    let free_plan_name = format!("Free Plan {}", common::unique_table_name("p"));
    let (status, body) = post(
        &app,
        "/api/plans",
        Some(&admin_token),
        Some(&json!({
            "name": free_plan_name,
            "price": 0,
            "total_calls": 10,
            "validity_days": 30
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create free plan: {body}");
    assert_eq!(body["price"], 0);

    let paid_plan_id = create_plan(&app, &admin_token, None, "Paid To Free Plan", 1, 10, 30).await;
    let (status, body) = put(
        &app,
        &format!("/api/plans/{paid_plan_id}"),
        Some(&admin_token),
        Some(&json!({ "price": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update plan to free: {body}");
    assert_eq!(body["price"], 0);
}

#[tokio::test]
async fn test_plan_create_validation() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("plan_val_adm")).await;

    for body in [
        json!({ "name": "Bad", "price": -1, "total_calls": 10, "validity_days": 30 }),
        json!({ "name": "Bad", "price": 100, "total_calls": 0, "validity_days": 30 }),
        json!({ "name": "Bad", "price": 100, "total_calls": 10, "validity_days": 0 }),
        json!({ "name": "Bad", "price": 100, "total_calls": 10, "validity_days": 30, "purchase_limit": 0 }),
        json!({ "name": "Bad", "price": 100, "total_calls": 10, "validity_days": 30, "purchase_limit": -1 }),
        json!({ "name": "  ", "price": 100, "total_calls": 10, "validity_days": 30 }),
    ] {
        let (status, resp) = post(&app, "/api/plans", Some(&admin_token), Some(&body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "should reject: {resp}");
    }
}

#[tokio::test]
async fn test_system_create_plan_requires_tenant_id() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("plan_sys_tid")).await;

    let (status, body) = post(
        &app,
        "/api/plans",
        Some(&sys_token),
        Some(&json!({ "name": "No Tenant", "price": 1, "total_calls": 1, "validity_days": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("tenant_id"),
        "error should mention tenant_id: {body}"
    );
}

// ── Tenant isolation ───────────────────────────────────────────

#[tokio::test]
async fn test_plan_tenant_isolation_for_admin() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("plan_iso_sys")).await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("plan_iso_adm")).await;

    // System creates a plan in a foreign tenant.
    let tenant_b = create_tenant(&app, &sys_token, &common::unique_table_name("plan_iso")).await;
    let foreign_plan = create_plan(
        &app,
        &sys_token,
        Some(&tenant_b),
        "Foreign Plan",
        100,
        10,
        30,
    )
    .await;

    // Admin (default tenant) must not see the foreign plan in listings.
    let (status, body) = get(&app, "/api/plans?page=1&per_page=100", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);
    let leaked = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == foreign_plan.as_str());
    assert!(!leaked, "admin must not see plans of other tenants");

    // Admin cannot update or delete the foreign plan — 404, no existence leak.
    let (status, _) = put(
        &app,
        &format!("/api/plans/{}", foreign_plan),
        Some(&admin_token),
        Some(&json!({ "price": 999i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = delete(
        &app,
        &format!("/api/plans/{}", foreign_plan),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // System can update it.
    let (status, _) = put(
        &app,
        &format!("/api/plans/{}", foreign_plan),
        Some(&sys_token),
        Some(&json!({ "price": 999i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_admin_cannot_assign_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("assign_x_sys")).await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("assign_x_adm")).await;

    let tenant_b = create_tenant(&app, &sys_token, &common::unique_table_name("assign_x")).await;
    let foreign_plan = create_plan(
        &app,
        &sys_token,
        Some(&tenant_b),
        "Foreign Plan",
        100,
        10,
        30,
    )
    .await;
    let foreign_user = create_user(
        &app,
        &sys_token,
        &common::unique_email("assign_x_user"),
        Some(&tenant_b),
    )
    .await;

    // Admin (default tenant) cannot assign to a foreign-tenant user.
    let (status, _) = post(
        &app,
        &format!("/api/users/{}/plans", foreign_user),
        Some(&admin_token),
        Some(&json!({ "plan_id": foreign_plan })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Nor list the foreign user's plans.
    let (status, _) = get(
        &app,
        &format!("/api/users/{}/plans", foreign_user),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // System can assign.
    let (status, body) = post(
        &app,
        &format!("/api/users/{}/plans", foreign_user),
        Some(&sys_token),
        Some(&json!({ "plan_id": foreign_plan })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "system assign should succeed: {body}"
    );
    assert_eq!(body["source"], "admin_grant");
}

// ── Assignment & self-service listing ──────────────────────────

#[tokio::test]
async fn test_assign_and_list_user_plans() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("assign_adm")).await;

    let user_email = common::unique_email("assign_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let plan_id = create_plan(&app, &admin_token, None, "Grant Plan", 500, 42, 7).await;

    let (status, body) = post(
        &app,
        &format!("/api/users/{}/plans", user_id),
        Some(&admin_token),
        Some(&json!({ "plan_id": plan_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "assign should succeed: {body}");
    assert_eq!(body["total_calls"], 42);
    assert_eq!(body["remaining_calls"], 42);
    assert_eq!(body["status"], "active");
    assert_eq!(body["source"], "admin_grant");

    // Admin view of the user's plans.
    let (status, body) = get(
        &app,
        &format!("/api/users/{}/plans", user_id),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // Self view.
    let user_token = axum_helpers::login(&app, &user_email).await;
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["plan_name"], "Grant Plan");
}

#[tokio::test]
async fn test_available_plans_only_active_own_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("avail_sys")).await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("avail_adm")).await;

    // Active + disabled plans in the default tenant, active plan in a foreign tenant.
    let active_plan = create_plan(&app, &admin_token, None, "Avail Active", 100, 10, 30).await;
    let disabled_plan = create_plan(&app, &admin_token, None, "Avail Disabled", 100, 10, 30).await;
    let (status, _) = put(
        &app,
        &format!("/api/plans/{}", disabled_plan),
        Some(&admin_token),
        Some(&json!({ "status": "disabled" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b = create_tenant(&app, &sys_token, &common::unique_table_name("avail")).await;
    let foreign_plan = create_plan(
        &app,
        &sys_token,
        Some(&tenant_b),
        "Avail Foreign",
        100,
        10,
        30,
    )
    .await;

    // A regular user in the default tenant sees only active default-tenant plans.
    let user_token =
        axum_helpers::register_and_login(&app, &common::unique_email("avail_user")).await;
    let (status, body) = get(&app, "/api/plans/available", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    let active = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|plan| plan["id"] == active_plan)
        .expect("active plan response");
    assert_eq!(active["purchases_used"], 0);
    assert!(ids.contains(&active_plan.as_str()), "active plan visible");
    assert!(
        !ids.contains(&disabled_plan.as_str()),
        "disabled plan hidden"
    );
    assert!(
        !ids.contains(&foreign_plan.as_str()),
        "foreign tenant plan hidden"
    );
}

// ── Purchase flow ──────────────────────────────────────────────

#[tokio::test]
async fn test_purchase_flow_with_balance() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("buy_adm")).await;

    let user_email = common::unique_email("buy_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    // Balance = 10.0 display units; plan price = 4.0.
    let (status, _) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({ "balance": 100_000_000_000i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plan_id = create_plan(&app, &admin_token, None, "Buy Plan", 40_000_000_000, 10, 30).await;

    let user_token = axum_helpers::login(&app, &user_email).await;

    // First purchase: balance 10.0 → 6.0
    let (status, body) = post(
        &app,
        &format!("/api/plans/{}/purchase", plan_id),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "purchase should succeed: {body}");
    assert_eq!(body["order"]["status"], "paid");
    assert_eq!(body["order"]["payment_method"], "balance");
    assert_eq!(body["order"]["amount"], 40_000_000_000i64);
    assert_eq!(body["order"]["user_email"], user_email.as_str());
    assert_eq!(body["user_plan"]["source"], "purchase");
    assert_eq!(body["user_plan"]["remaining_calls"], 10);
    assert_eq!(body["balance"], 60_000_000_000i64);

    // Second purchase: balance 6.0 → 2.0
    let (status, body) = post(
        &app,
        &format!("/api/plans/{}/purchase", plan_id),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["balance"], 20_000_000_000i64);

    // Third purchase: insufficient balance
    let (status, body) = post(
        &app,
        &format!("/api/plans/{}/purchase", plan_id),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "insufficient_balance");

    // Own orders show the two paid purchases.
    let (status, body) = get(&app, "/api/users/me/orders", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);

    // Deleting the plan template must not affect held instances (snapshot design).
    let (status, _) = delete(&app, &format!("/api/plans/{}", plan_id), Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let actives = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["status"] == "active")
        .count();
    assert_eq!(actives, 2, "purchased instances survive template deletion");
}

#[tokio::test]
async fn test_free_plan_purchase_creates_zero_amount_order_without_changing_balance() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("free_buy_adm")).await;

    let user_email = common::unique_email("free_buy_user");
    create_user(&app, &admin_token, &user_email, None).await;
    let plan_id = create_plan(&app, &admin_token, None, "Free Buy Plan", 0, 10, 30).await;
    let user_token = axum_helpers::login(&app, &user_email).await;

    let (status, body) = post(
        &app,
        &format!("/api/plans/{plan_id}/purchase"),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "purchase free plan: {body}");
    assert_eq!(body["order"]["amount"], 0);
    assert_eq!(body["order"]["status"], "paid");
    assert_eq!(body["user_plan"]["remaining_calls"], 10);
    assert_eq!(body["balance"], 0);

    let (status, body) = get(&app, "/api/users/me/orders", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK, "list free plan order: {body}");
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["amount"], 0);
}

#[tokio::test]
async fn test_purchase_limit_excludes_admin_grants_persists_after_refund_and_can_be_cleared() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("purchase_limit_adm"))
            .await;

    let user_email = common::unique_email("purchase_limit_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let (status, body) = put(
        &app,
        &format!("/api/users/{user_id}/balance"),
        Some(&admin_token),
        Some(&json!({ "balance": 10i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fund user: {body}");

    let plan_id =
        create_plan_with_limit(&app, &admin_token, None, "Limited Plan", 1, 10, 30, Some(1)).await;

    // An administrator grant creates an instance but does not consume the
    // self-service purchase allowance.
    let (status, body) = post(
        &app,
        &format!("/api/users/{user_id}/plans"),
        Some(&admin_token),
        Some(&json!({ "plan_id": plan_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin grant: {body}");
    assert_eq!(body["source"], "admin_grant");

    let user_token = axum_helpers::login(&app, &user_email).await;
    let purchase_uri = format!("/api/plans/{plan_id}/purchase");
    let (status, body) = post(&app, &purchase_uri, Some(&user_token), None).await;
    assert_eq!(status, StatusCode::OK, "first purchase: {body}");
    assert_eq!(body["balance"], 9);

    let (status, available) = get(&app, "/api/plans/available", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK, "available plans: {available}");
    let limited = available["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|plan| plan["id"] == plan_id)
        .expect("limited plan remains visible");
    assert_eq!(limited["purchases_used"], 1);
    assert_eq!(limited["purchase_limit"], 1);

    // Changing the audit-order status is record keeping only. The durable
    // purchase instance remains and therefore still occupies the allowance.
    let (status, orders) = get(
        &app,
        &format!("/api/orders?page=1&per_page=10&user_id={user_id}"),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list orders: {orders}");
    let order_id = orders["items"][0]["id"].as_str().unwrap();
    let (status, body) = put(
        &app,
        &format!("/api/orders/{order_id}"),
        Some(&admin_token),
        Some(&json!({ "status": "refunded" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "refund order record: {body}");

    let (status, body) = post(&app, &purchase_uri, Some(&user_token), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "limited purchase: {body}");
    assert_eq!(body["error"], "purchase_limit_reached");

    // JSON null explicitly removes the limit; an omitted field would leave it
    // unchanged (covered by the request deserialization unit test).
    let (status, body) = put(
        &app,
        &format!("/api/plans/{plan_id}"),
        Some(&admin_token),
        Some(&json!({ "purchase_limit": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "clear purchase limit: {body}");
    assert!(body["purchase_limit"].is_null());

    let (status, body) = post(&app, &purchase_uri, Some(&user_token), None).await;
    assert_eq!(status, StatusCode::OK, "purchase after clearing: {body}");
    assert_eq!(body["balance"], 8);

    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK, "list instances: {body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(
        items
            .iter()
            .filter(|item| item["source"] == "purchase")
            .count(),
        2
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| item["source"] == "admin_grant")
            .count(),
        1
    );
}

#[tokio::test]
async fn test_purchase_limit_is_atomic_without_redis_purchase_lock() {
    let app = axum_helpers::create_app().await;
    let admin_token = axum_helpers::create_admin_and_login(
        &app,
        &common::unique_email("purchase_limit_race_adm"),
    )
    .await;

    let user_email = common::unique_email("purchase_limit_race_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let (status, body) = put(
        &app,
        &format!("/api/users/{user_id}/balance"),
        Some(&admin_token),
        Some(&json!({ "balance": 10i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fund user: {body}");
    let plan_id = create_plan_with_limit(
        &app,
        &admin_token,
        None,
        "Atomic Limited Plan",
        1,
        10,
        30,
        Some(1),
    )
    .await;

    // Call the service directly so Redis's duplicate-submit lock cannot hide
    // a race in the database enforcement. The buyer row lock must serialize
    // count + insert into one atomic decision.
    let db = common::create_test_db_and_run_migrations().await;
    let service_a = ains_server::services::plan::PlanService::new(db.clone());
    let service_b = ains_server::services::plan::PlanService::new(db);
    let user_id: i64 = user_id.parse().unwrap();
    let plan_id: i64 = plan_id.parse().unwrap();
    let (result_a, result_b) = tokio::join!(
        service_a.purchase(user_id, plan_id),
        service_b.purchase(user_id, plan_id),
    );
    let outcomes = [result_a, result_b];
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(
                result,
                Err(ains_server::services::PlanError::PurchaseLimitReached)
            ))
            .count(),
        1,
        "one concurrent transaction must observe the committed purchase"
    );
}

#[tokio::test]
async fn test_purchase_cross_tenant_plan_returns_404() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("buy_x_sys")).await;

    let tenant_b = create_tenant(&app, &sys_token, &common::unique_table_name("buy_x")).await;
    let foreign_plan = create_plan(&app, &sys_token, Some(&tenant_b), "Foreign Buy", 1, 1, 1).await;

    // A default-tenant user cannot buy a foreign tenant's plan.
    let user_token =
        axum_helpers::register_and_login(&app, &common::unique_email("buy_x_user")).await;
    let (status, _) = post(
        &app,
        &format!("/api/plans/{}/purchase", foreign_plan),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_purchase_disabled_plan_returns_404() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("buy_dis_adm")).await;

    let plan_id = create_plan(&app, &admin_token, None, "Disabled Buy", 1, 1, 1).await;
    let (status, _) = put(
        &app,
        &format!("/api/plans/{}", plan_id),
        Some(&admin_token),
        Some(&json!({ "status": "disabled" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Disabled plans are not purchasable — reads as NotFound.
    let user_token =
        axum_helpers::register_and_login(&app, &common::unique_email("buy_dis_user")).await;
    let (status, _) = post(
        &app,
        &format!("/api/plans/{}/purchase", plan_id),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_concurrent_purchase_no_double_spend() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("race_adm")).await;

    // Balance covers exactly ONE purchase.
    let user_email = common::unique_email("race_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let (status, _) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({ "balance": 40_000_000_000i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plan_id = create_plan(&app, &admin_token, None, "Race Plan", 40_000_000_000, 5, 30).await;

    let user_token = axum_helpers::login(&app, &user_email).await;
    let uri = format!("/api/plans/{}/purchase", plan_id);

    // Fire two purchases concurrently. Two independent layers may stop the
    // loser: the per-user purchase lock (409 purchase_in_progress) or — when
    // Redis is unavailable and the lock is skipped — the row-lock balance
    // check (400 insufficient_balance). Exactly one may succeed either way.
    let (r1, r2) = tokio::join!(
        post(&app, &uri, Some(&user_token), None),
        post(&app, &uri, Some(&user_token), None),
    );
    let outcomes = [r1, r2];
    let successes = outcomes
        .iter()
        .filter(|(s, _)| *s == StatusCode::OK)
        .count();
    let rejected = outcomes
        .iter()
        .filter(|(s, b)| {
            (*s == StatusCode::BAD_REQUEST && b["error"] == "insufficient_balance")
                || (*s == StatusCode::CONFLICT && b["error"] == "purchase_in_progress")
        })
        .count();
    assert_eq!(
        successes, 1,
        "exactly one purchase must succeed: {outcomes:?}"
    );
    assert_eq!(
        rejected, 1,
        "the loser must see purchase_in_progress or insufficient_balance: {outcomes:?}"
    );

    // Exactly one order and one instance; balance fully spent.
    let (status, body) = get(&app, "/api/users/me/orders", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1, "double spend must not create two orders");
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_expired_instance_not_consumable_and_shows_expired() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("exp_adm")).await;

    let user_email = common::unique_email("exp_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let plan_id = create_plan(&app, &admin_token, None, "Expiring Plan", 1, 10, 30).await;
    let (status, _) = post(
        &app,
        &format!("/api/users/{}/plans", user_id),
        Some(&admin_token),
        Some(&json!({ "plan_id": plan_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Force the instance into the past — expiry is evaluated at query time,
    // so no background job is involved.
    {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
        let db = common::create_test_db_and_run_migrations().await;
        let uid: i64 = user_id.parse().unwrap();
        db.write_conn()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE user_plans SET expires_at = NOW() - INTERVAL '1 day' WHERE user_id = $1",
                [uid.into()],
            ))
            .await
            .expect("failed to expire user plan");
    }

    // Expired instances hold remaining calls but must not open the gate.
    let user_token = axum_helpers::login(&app, &user_email).await;
    let (status, body) = post(
        &app,
        "/api/ai/response",
        Some(&user_token),
        Some(&json!({ "input": "hello" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["code"], "no_active_plan");

    // Derived state reads "expired" (takes precedence over exhausted).
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"].as_array().unwrap()[0];
    assert_eq!(item["status"], "expired");
    assert_eq!(item["remaining_calls"], 10, "calls untouched by expiry");
}

#[tokio::test]
async fn test_admin_tenant_filter_param_is_ignored() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("filt_sys")).await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("filt_adm")).await;

    let tenant_b = create_tenant(&app, &sys_token, &common::unique_table_name("filt")).await;
    let foreign_plan =
        create_plan(&app, &sys_token, Some(&tenant_b), "Filter Foreign", 1, 1, 1).await;

    // The tenant_id query param is a system-only convenience; admin passing a
    // foreign tenant must still be forced onto its own tenant (last line of
    // defense against cross-tenant read leaks).
    let (status, body) = get(
        &app,
        &format!("/api/plans?page=1&per_page=100&tenant_id={}", tenant_b),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let leaked = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == foreign_plan.as_str());
    assert!(!leaked, "admin tenant_id filter must be ignored: {body}");

    // system, by contrast, can use the filter.
    let (status, body) = get(
        &app,
        &format!("/api/plans?page=1&per_page=100&tenant_id={}", tenant_b),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let found = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == foreign_plan.as_str());
    assert!(found, "system tenant filter should work: {body}");
}

// ── /api/ai/response plan-quota gate ───────────────────────────

#[tokio::test]
async fn test_responses_rejects_user_without_plan() {
    let app = axum_helpers::create_app().await;
    let user_token =
        axum_helpers::register_and_login(&app, &common::unique_email("quota_no_plan")).await;

    let (status, body) = post(
        &app,
        "/api/ai/response",
        Some(&user_token),
        Some(&json!({ "input": "hello" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no plan → 403: {body}");
    assert_eq!(body["error"]["code"], "no_active_plan");
}

#[tokio::test]
async fn test_responses_admin_exempt_from_plan_quota() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("quota_adm")).await;

    // Admin holds no plan, yet must pass the plan-quota layer. The request may
    // still fail later (no channel / upstream), but never with no_active_plan.
    let (_status, body) = post(
        &app,
        "/api/ai/response",
        Some(&admin_token),
        Some(&json!({ "input": "hello" })),
    )
    .await;
    assert_ne!(body["error"]["code"], "no_active_plan");
}

#[tokio::test]
async fn test_responses_consume_calls_until_exhausted() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("quota_sys")).await;

    // Isolated tenant with a mock upstream channel: consumption must go
    // through a successful proxy path, because NoChannel failures refund.
    let port = start_mock_upstream().await;
    let tenant = create_tenant(&app, &sys_token, &common::unique_table_name("quota")).await;
    create_mock_channel(&app, &sys_token, &tenant, port).await;
    let plan_id = create_plan(&app, &sys_token, Some(&tenant), "Two Calls", 1, 2, 30).await;
    let user_email = common::unique_email("quota_user");
    let user_id = create_user(&app, &sys_token, &user_email, Some(&tenant)).await;
    let (status, _) = post(
        &app,
        &format!("/api/users/{}/plans", user_id),
        Some(&sys_token),
        Some(&json!({ "plan_id": plan_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let user_token = axum_helpers::login(&app, &user_email).await;

    // Two calls consume the plan (each succeeds against the mock upstream).
    for _ in 0..2 {
        let (status, body) = post(
            &app,
            "/api/ai/response",
            Some(&user_token),
            Some(&json!({ "input": "hello" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
    }

    // Third call: plan exhausted.
    let (status, body) = post(
        &app,
        "/api/ai/response",
        Some(&user_token),
        Some(&json!({ "input": "hello" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "no_active_plan");

    // Remaining calls visible via self-service listing.
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"].as_array().unwrap()[0];
    assert_eq!(item["remaining_calls"], 0);
    assert_eq!(item["status"], "exhausted");
}

/// NoChannel failures must refund the consumed call: a missing channel is
/// an operator-side misconfiguration and must not burn user quota.
#[tokio::test]
async fn test_no_channel_failure_refunds_call() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("refund_sys")).await;

    // Isolated tenant WITHOUT channels.
    let tenant = create_tenant(&app, &sys_token, &common::unique_table_name("refund")).await;
    let plan_id = create_plan(&app, &sys_token, Some(&tenant), "Refund Plan", 1, 2, 30).await;
    let user_email = common::unique_email("refund_user");
    let user_id = create_user(&app, &sys_token, &user_email, Some(&tenant)).await;
    let (status, _) = post(
        &app,
        &format!("/api/users/{}/plans", user_id),
        Some(&sys_token),
        Some(&json!({ "plan_id": plan_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let user_token = axum_helpers::login(&app, &user_email).await;

    // More calls than the plan holds — every one fails with 503 NoChannel
    // and gets refunded; the gate must never close with no_active_plan.
    for _ in 0..3 {
        let (status, body) = post(
            &app,
            "/api/ai/response",
            Some(&user_token),
            Some(&json!({ "input": "hello" })),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    }

    // Quota fully restored.
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"].as_array().unwrap()[0];
    assert_eq!(item["remaining_calls"], 2, "NoChannel must refund: {body}");
    assert_eq!(item["status"], "active");
}

/// Regression guard for the consume_call locking strategy: with plain
/// `FOR UPDATE` (not SKIP LOCKED), concurrent requests racing on a user's
/// single active instance must serialize and BOTH consume successfully —
/// never a false 403 no_active_plan while calls remain.
#[tokio::test]
async fn test_concurrent_consume_single_instance_no_false_403() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("cc_sys")).await;

    // Isolated tenant with a mock upstream channel (NoChannel would refund).
    let port = start_mock_upstream().await;
    let tenant = create_tenant(&app, &sys_token, &common::unique_table_name("cc")).await;
    create_mock_channel(&app, &sys_token, &tenant, port).await;
    let plan_id = create_plan(&app, &sys_token, Some(&tenant), "Concurrent Plan", 1, 2, 30).await;
    let user_email = common::unique_email("cc_user");
    let user_id = create_user(&app, &sys_token, &user_email, Some(&tenant)).await;
    let (status, _) = post(
        &app,
        &format!("/api/users/{}/plans", user_id),
        Some(&sys_token),
        Some(&json!({ "plan_id": plan_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let user_token = axum_helpers::login(&app, &user_email).await;
    let body = json!({ "input": "hello" });

    // Two concurrent calls against remaining_calls = 2: both must pass the
    // quota gate and succeed — neither may see a false 403.
    let (r1, r2) = tokio::join!(
        post(&app, "/api/ai/response", Some(&user_token), Some(&body)),
        post(&app, "/api/ai/response", Some(&user_token), Some(&body)),
    );
    for (status, resp) in [&r1, &r2] {
        assert_eq!(
            *status,
            StatusCode::OK,
            "concurrent call with remaining quota must not be rejected: {resp}"
        );
    }

    // Both calls consumed — the third one hits the closed gate.
    let (status, resp) = post(&app, "/api/ai/response", Some(&user_token), Some(&body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {resp}");
    assert_eq!(resp["error"]["code"], "no_active_plan");
}

/// Multi-instance consumption order: the earliest-expiring instance is
/// drained first, and once it runs out concurrent consumers fall through
/// to the next instance (READ COMMITTED predicate re-check path).
#[tokio::test]
async fn test_concurrent_consume_prefers_earliest_and_falls_through() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("mi_sys")).await;

    let port = start_mock_upstream().await;
    let tenant = create_tenant(&app, &sys_token, &common::unique_table_name("mi")).await;
    create_mock_channel(&app, &sys_token, &tenant, port).await;

    // Instance A: expires first (1 day), a single call left.
    // Instance B: expires later (30 days), plenty of calls.
    let plan_a = create_plan(&app, &sys_token, Some(&tenant), "Early Plan", 1, 1, 1).await;
    let plan_b = create_plan(&app, &sys_token, Some(&tenant), "Late Plan", 1, 5, 30).await;
    let user_email = common::unique_email("mi_user");
    let user_id = create_user(&app, &sys_token, &user_email, Some(&tenant)).await;
    for plan_id in [&plan_a, &plan_b] {
        let (status, _) = post(
            &app,
            &format!("/api/users/{}/plans", user_id),
            Some(&sys_token),
            Some(&json!({ "plan_id": plan_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let user_token = axum_helpers::login(&app, &user_email).await;
    let body = json!({ "input": "hello" });

    // Two concurrent calls: one drains instance A, the other must fall
    // through to instance B instead of failing.
    let (r1, r2) = tokio::join!(
        post(&app, "/api/ai/response", Some(&user_token), Some(&body)),
        post(&app, "/api/ai/response", Some(&user_token), Some(&body)),
    );
    for (status, resp) in [&r1, &r2] {
        assert_eq!(*status, StatusCode::OK, "both must succeed: {resp}");
    }

    // A drained to 0 (earliest-expiry preference), B charged exactly once.
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    let remaining_of = |name: &str| {
        items
            .iter()
            .find(|p| p["plan_name"] == name)
            .map(|p| p["remaining_calls"].as_i64().unwrap())
            .unwrap()
    };
    assert_eq!(
        remaining_of("Early Plan"),
        0,
        "earliest instance drained first"
    );
    assert_eq!(
        remaining_of("Late Plan"),
        4,
        "second consumer falls through"
    );
}

/// Purchasing must invalidate the cached profile so /users/me immediately
/// reflects the deducted balance (guards the user:{id} cache-key coupling).
#[tokio::test]
async fn test_purchase_refreshes_cached_me_balance() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("cache_adm")).await;

    let user_email = common::unique_email("cache_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let (status, _) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({ "balance": 100_000_000_000i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plan_id = create_plan(
        &app,
        &admin_token,
        None,
        "Cache Plan",
        40_000_000_000,
        10,
        30,
    )
    .await;

    let user_token = axum_helpers::login(&app, &user_email).await;

    // Warm the profile cache with the pre-purchase balance.
    let (status, me) = get(&app, "/api/users/me", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["balance"], 100_000_000_000i64);

    let (status, _) = post(
        &app,
        &format!("/api/plans/{}/purchase", plan_id),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The cached profile must have been invalidated by the purchase.
    let (status, me) = get(&app, "/api/users/me", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        me["balance"], 60_000_000_000i64,
        "purchase must invalidate the cached balance: {me}"
    );
}

/// Admins of a disabled tenant must not manage plans or orders
/// (parity with the channel module's active-tenant gate).
#[tokio::test]
async fn test_disabled_tenant_admin_cannot_manage_plans_or_orders() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("dis_sys")).await;

    // Tenant B with its own admin.
    let tenant_b = create_tenant(&app, &sys_token, &common::unique_table_name("dis")).await;
    let admin_email = common::unique_email("dis_adm");
    let (status, _) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": admin_email,
            "password": common::DEFAULT_TEST_PASSWORD,
            "name": "Disabled Tenant Admin",
            "role": "admin",
            "tenant_id": tenant_b,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let admin_token = axum_helpers::login(&app, &admin_email).await;

    // A regular user in tenant B (for the /plans/available gate check) and
    // a purchasable plan created while the tenant is still active.
    let member_email = common::unique_email("dis_member");
    create_user(&app, &sys_token, &member_email, Some(&tenant_b)).await;
    let member_token = axum_helpers::login(&app, &member_email).await;
    let plan_b = create_plan(
        &app,
        &sys_token,
        Some(&tenant_b),
        "Disabled Gate Plan",
        1,
        1,
        30,
    )
    .await;

    // While active, the admin can list plans and the user can browse.
    let (status, _) = get(&app, "/api/plans", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&app, "/api/plans/available", Some(&member_token)).await;
    assert_eq!(status, StatusCode::OK);

    // Disable the tenant — management access must close.
    let (status, _) = put(
        &app,
        &format!("/api/tenants/{}", tenant_b),
        Some(&sys_token),
        Some(&json!({ "status": "disabled" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(&app, "/api/plans", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "plans: {body}");
    let (status, body) = get(&app, "/api/orders", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "orders: {body}");
    let (status, body) = post(
        &app,
        "/api/plans",
        Some(&admin_token),
        Some(&json!({ "name": "x", "price": 1, "total_calls": 1, "validity_days": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "create plan: {body}");

    // The purchase-side browsing gate closes too.
    let (status, body) = get(&app, "/api/plans/available", Some(&member_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "available: {body}");
    // …and so does the purchase endpoint itself.
    let (status, body) = post(
        &app,
        &format!("/api/plans/{}/purchase", plan_b),
        Some(&member_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "purchase: {body}");

    // system remains unaffected.
    let (status, _) = get(&app, "/api/plans?page=1&per_page=10", Some(&sys_token)).await;
    assert_eq!(status, StatusCode::OK);
}

/// Sequential double submits are two intentional purchases: instances stack,
/// two paid orders are recorded, and the balance is deducted exactly twice.
/// (Rapid duplicates are suppressed by the per-user purchase lock — see
/// test_concurrent_purchase_no_double_spend.)
#[tokio::test]
async fn test_sequential_repurchase_stacks_instances() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("stack_adm")).await;

    let user_email = common::unique_email("stack_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let (status, _) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({ "balance": 100_000_000_000i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plan_id = create_plan(
        &app,
        &admin_token,
        None,
        "Stack Plan",
        40_000_000_000,
        5,
        30,
    )
    .await;

    let user_token = axum_helpers::login(&app, &user_email).await;
    let uri = format!("/api/plans/{}/purchase", plan_id);

    // Two sequential purchases: each deducts the full price exactly once.
    for expected_balance in [60_000_000_000i64, 20_000_000_000] {
        let (status, body) = post(&app, &uri, Some(&user_token), None).await;
        assert_eq!(status, StatusCode::OK, "repurchase must succeed: {body}");
        assert_eq!(body["balance"], expected_balance, "exact deduction: {body}");
    }

    // Two orders and two stacked instances document the semantics.
    let (status, body) = get(&app, "/api/users/me/orders", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2, "each submit records its own order");
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["items"].as_array().unwrap().len(),
        2,
        "instances stack instead of merging"
    );
}

/// Race on the LAST remaining call: with remaining_calls = 1, two concurrent
/// requests must resolve to exactly one success and one 403 no_active_plan —
/// the counter must end at exactly 0, never negative (exercises the atomic
/// UPDATE + existence-probe retry loop; the DB CHECK is only the backstop).
#[tokio::test]
async fn test_concurrent_consume_last_call_exactly_one_succeeds() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("last_sys")).await;

    // Isolated tenant with a mock upstream channel (NoChannel would refund).
    let port = start_mock_upstream().await;
    let tenant = create_tenant(&app, &sys_token, &common::unique_table_name("last")).await;
    create_mock_channel(&app, &sys_token, &tenant, port).await;
    let plan_id = create_plan(&app, &sys_token, Some(&tenant), "Last Call Plan", 1, 1, 30).await;
    let user_email = common::unique_email("last_user");
    let user_id = create_user(&app, &sys_token, &user_email, Some(&tenant)).await;
    let (status, _) = post(
        &app,
        &format!("/api/users/{}/plans", user_id),
        Some(&sys_token),
        Some(&json!({ "plan_id": plan_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let user_token = axum_helpers::login(&app, &user_email).await;
    let body = json!({ "input": "hello" });

    let (r1, r2) = tokio::join!(
        post(&app, "/api/ai/response", Some(&user_token), Some(&body)),
        post(&app, "/api/ai/response", Some(&user_token), Some(&body)),
    );
    let outcomes = [r1, r2];
    let successes = outcomes
        .iter()
        .filter(|(s, _)| *s == StatusCode::OK)
        .count();
    let rejected = outcomes
        .iter()
        .filter(|(s, b)| *s == StatusCode::FORBIDDEN && b["error"]["code"] == "no_active_plan")
        .count();
    assert_eq!(
        successes, 1,
        "exactly one call may consume the last unit: {outcomes:?}"
    );
    assert_eq!(
        rejected, 1,
        "the loser must see no_active_plan: {outcomes:?}"
    );

    // Counter drained to exactly zero — never negative.
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"].as_array().unwrap()[0];
    assert_eq!(item["remaining_calls"], 0, "counter must end at 0: {body}");
    assert_eq!(item["status"], "exhausted");
}

/// The `remaining_calls < total_calls` guard makes refund_call idempotent:
/// anomalous double compensation must never push the counter past
/// total_calls (service-level test — the HTTP path can only refund once
/// per consume, so the double refund is driven directly).
#[tokio::test]
async fn test_refund_call_never_exceeds_total() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("rfd_adm")).await;

    let user_email = common::unique_email("rfd_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let plan_id = create_plan(&app, &admin_token, None, "Refund Guard Plan", 1, 2, 30).await;
    let (status, inst) = post(
        &app,
        &format!("/api/users/{}/plans", user_id),
        Some(&admin_token),
        Some(&json!({ "plan_id": plan_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "grant should succeed: {inst}");
    let instance_id: i64 = inst["id"].as_str().expect("instance id").parse().unwrap();

    // Drive consume + double refund directly against the service.
    let db = common::create_test_db_and_run_migrations().await;
    let svc = ains_server::services::plan::PlanService::new(db);
    let uid: i64 = user_id.parse().unwrap();
    let consumed = svc.consume_call(uid).await.expect("consume should succeed");
    assert_eq!(consumed, instance_id, "the granted instance is consumed");
    svc.refund_call(consumed).await.expect("first refund");
    svc.refund_call(consumed)
        .await
        .expect("second refund must be a silent no-op");

    // remaining_calls is restored to total_calls — and not one past it.
    let user_token = axum_helpers::login(&app, &user_email).await;
    let (status, body) = get(&app, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"].as_array().unwrap()[0];
    assert_eq!(
        item["remaining_calls"], 2,
        "double refund must not exceed total_calls: {body}"
    );
}

/// A failed purchase must release the per-user purchase lock before the
/// handler responds: insufficient balance (400) followed by an immediately
/// funded retry must succeed — never a spurious 409 purchase_in_progress
/// from a stale lock (pins the release-before-map_err ordering).
#[tokio::test]
async fn test_failed_purchase_releases_lock_for_retry() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("relock_adm")).await;

    // Fresh user starts with zero balance.
    let user_email = common::unique_email("relock_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let plan_id = create_plan(
        &app,
        &admin_token,
        None,
        "Relock Plan",
        40_000_000_000,
        5,
        30,
    )
    .await;

    let user_token = axum_helpers::login(&app, &user_email).await;
    let uri = format!("/api/plans/{}/purchase", plan_id);

    let (status, body) = post(&app, &uri, Some(&user_token), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unfunded purchase: {body}");
    assert_eq!(body["error"], "insufficient_balance");

    // Fund and retry immediately — the failed attempt's lock must be gone.
    let (status, _) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({ "balance": 40_000_000_000i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(&app, &uri, Some(&user_token), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "retry after a failed purchase must not hit a stale lock: {body}"
    );
}

/// Documents the per-user (not per-user+plan) purchase-lock scope: two
/// concurrent purchases of DIFFERENT plans by the same user either both
/// succeed (serialized fast enough) or the second is suppressed with 409 —
/// never any other failure. Guards against a future refactor silently
/// narrowing the lock key to per-plan.
#[tokio::test]
async fn test_concurrent_purchase_of_different_plans_is_per_user_scoped() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("scope_adm")).await;

    // Balance covers both plans comfortably — insufficient_balance is
    // impossible, so any rejection must come from the purchase lock.
    let user_email = common::unique_email("scope_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;
    let (status, _) = put(
        &app,
        &format!("/api/users/{}/balance", user_id),
        Some(&admin_token),
        Some(&json!({ "balance": 100_000_000_000i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plan_a = create_plan(
        &app,
        &admin_token,
        None,
        "Scope Plan A",
        10_000_000_000,
        5,
        30,
    )
    .await;
    let plan_b = create_plan(
        &app,
        &admin_token,
        None,
        "Scope Plan B",
        20_000_000_000,
        5,
        30,
    )
    .await;

    let user_token = axum_helpers::login(&app, &user_email).await;
    let uri_a = format!("/api/plans/{}/purchase", plan_a);
    let uri_b = format!("/api/plans/{}/purchase", plan_b);
    let (r1, r2) = tokio::join!(
        post(&app, &uri_a, Some(&user_token), None),
        post(&app, &uri_b, Some(&user_token), None),
    );
    let outcomes = [r1, r2];
    for (status, body) in &outcomes {
        assert!(
            *status == StatusCode::OK
                || (*status == StatusCode::CONFLICT && body["error"] == "purchase_in_progress"),
            "only success or the per-user lock 409 is acceptable: {status} {body}"
        );
    }
    let successes = outcomes
        .iter()
        .filter(|(s, _)| *s == StatusCode::OK)
        .count();
    assert!(
        successes >= 1,
        "at least one purchase must succeed: {outcomes:?}"
    );

    // Bookkeeping matches whichever interleaving occurred.
    let (status, body) = get(&app, "/api/users/me/orders", Some(&user_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total"].as_u64().unwrap() as usize,
        successes,
        "each successful purchase records exactly one order: {body}"
    );
}
