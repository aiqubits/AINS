#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for payment order management (admin CRUD, tenant
//! isolation, self-service order listing).

use ains_axum::{Body, BodyExt, Method, Router, StatusCode};
use serde_json::{Value, json};

mod common;
use common::axum as axum_helpers;

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

/// Create a user via API and return its ID (as string).
async fn create_user(app: &Router, token: &str, email: &str, tenant_id: Option<&str>) -> String {
    let mut body = json!({
        "email": email,
        "password": common::DEFAULT_TEST_PASSWORD,
        "name": "Order Test User",
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

#[tokio::test]
async fn test_order_crud_as_admin() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("ord_crud_adm")).await;

    let user_email = common::unique_email("ord_crud_user");
    let user_id = create_user(&app, &admin_token, &user_email, None).await;

    // Create a pending wechat order (manual back-fill scenario).
    let (status, body) = post(
        &app,
        "/api/orders",
        Some(&admin_token),
        Some(&json!({
            "user_id": user_id,
            "amount": 30_000_000_000i64,
            "status": "pending",
            "payment_method": "wechat",
            "external_txn_id": "wx_txn_123",
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "order creation should succeed: {body}"
    );
    let order_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["user_email"], user_email.as_str());
    assert_eq!(body["status"], "pending");
    assert_eq!(body["payment_method"], "wechat");
    assert_eq!(body["external_txn_id"], "wx_txn_123");
    assert!(
        body.get("paid_at").is_none() || body["paid_at"].is_null(),
        "pending order has no paid_at"
    );

    // Get by ID.
    let (status, body) = get(
        &app,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], order_id.as_str());

    // Update: transition to paid stamps paid_at.
    let (status, body) = put(
        &app,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
        Some(&json!({ "status": "paid" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "order update should succeed: {body}"
    );
    assert_eq!(body["status"], "paid");
    assert!(body["paid_at"].is_string(), "paid_at stamped: {body}");

    // Record-keeping refund: no side effects expected, just the status flip.
    let (status, body) = put(
        &app,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
        Some(&json!({ "status": "refunded" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "refunded");

    // Delete: orders are audit records — admin is rejected, system deletes.
    let (status, body) = delete(
        &app,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "admin must not delete order records: {body}"
    );
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("ord_crud_sys")).await;
    let (status, _) = delete(&app, &format!("/api/orders/{}", order_id), Some(&sys_token)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(
        &app,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_paid_at_not_restamped_on_second_paid_transition() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("ord_stamp_adm")).await;
    let user_id = create_user(
        &app,
        &admin_token,
        &common::unique_email("ord_stamp_user"),
        None,
    )
    .await;

    let (status, body) = post(
        &app,
        "/api/orders",
        Some(&admin_token),
        Some(&json!({ "user_id": user_id, "amount": 10, "status": "pending" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let order_id = body["id"].as_str().unwrap().to_string();

    // First paid transition stamps paid_at.
    let (status, body) = put(
        &app,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
        Some(&json!({ "status": "paid" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first_paid_at = body["paid_at"]
        .as_str()
        .expect("paid_at stamped")
        .to_string();

    // paid -> refunded -> paid again: the original stamp must be preserved.
    let (status, _) = put(
        &app,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
        Some(&json!({ "status": "refunded" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = put(
        &app,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
        Some(&json!({ "status": "paid" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["paid_at"].as_str().unwrap(),
        first_paid_at,
        "paid_at must not be re-stamped on repeated paid transitions"
    );
}

#[tokio::test]
async fn test_order_validation() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("ord_val_adm")).await;
    let user_id = create_user(
        &app,
        &admin_token,
        &common::unique_email("ord_val_user"),
        None,
    )
    .await;

    // Invalid status.
    let (status, _) = post(
        &app,
        "/api/orders",
        Some(&admin_token),
        Some(&json!({ "user_id": user_id, "amount": 1, "status": "shipped" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Invalid payment method.
    let (status, _) = post(
        &app,
        "/api/orders",
        Some(&admin_token),
        Some(&json!({ "user_id": user_id, "amount": 1, "payment_method": "cash" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Negative amount.
    let (status, _) = post(
        &app,
        "/api/orders",
        Some(&admin_token),
        Some(&json!({ "user_id": user_id, "amount": -5 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Nonexistent user.
    let (status, _) = post(
        &app,
        "/api/orders",
        Some(&admin_token),
        Some(&json!({ "user_id": "1", "amount": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Non-numeric user_id filter on the list endpoint is a client error,
    // not a silent no-filter fallback.
    let (status, body) = get(&app, "/api/orders?user_id=not-a-number", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

    // Empty user_id filter is tolerated (treated as absent).
    let (status, _) = get(&app, "/api/orders?user_id=", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_order_tenant_isolation_for_admin() {
    let app = axum_helpers::create_app().await;
    let sys_token =
        axum_helpers::create_system_and_login(&app, &common::unique_email("ord_iso_sys")).await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("ord_iso_adm")).await;

    // Foreign tenant + user + order created by system.
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({ "name": common::unique_table_name("ord_iso") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b = body["id"].as_str().unwrap().to_string();
    let foreign_user = create_user(
        &app,
        &sys_token,
        &common::unique_email("ord_iso_user"),
        Some(&tenant_b),
    )
    .await;
    let (status, body) = post(
        &app,
        "/api/orders",
        Some(&sys_token),
        Some(&json!({ "user_id": foreign_user, "amount": 100 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let foreign_order = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["tenant_id"], tenant_b.as_str());

    // Admin (default tenant) must not see the foreign order in listings.
    let (status, body) = get(&app, "/api/orders?page=1&per_page=100", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);
    let leaked = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["id"] == foreign_order.as_str());
    assert!(!leaked, "admin must not see orders of other tenants");

    // Get / update the foreign order — 404 for admin (no existence leak);
    // delete is rejected outright with 403 (system-only, checked first).
    let (status, _) = get(
        &app,
        &format!("/api/orders/{}", foreign_order),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = put(
        &app,
        &format!("/api/orders/{}", foreign_order),
        Some(&admin_token),
        Some(&json!({ "status": "cancelled" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = delete(
        &app,
        &format!("/api/orders/{}", foreign_order),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Admin cannot create an order for a foreign-tenant user either.
    let (status, _) = post(
        &app,
        "/api/orders",
        Some(&admin_token),
        Some(&json!({ "user_id": foreign_user, "amount": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // System sees everything.
    let (status, body) = get(
        &app,
        &format!("/api/orders/{}", foreign_order),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], foreign_order.as_str());
}

#[tokio::test]
async fn test_my_orders_only_own() {
    let app = axum_helpers::create_app().await;
    let admin_token =
        axum_helpers::create_admin_and_login(&app, &common::unique_email("ord_my_adm")).await;

    let email_a = common::unique_email("ord_my_a");
    let email_b = common::unique_email("ord_my_b");
    let user_a = create_user(&app, &admin_token, &email_a, None).await;
    let _user_b = create_user(&app, &admin_token, &email_b, None).await;

    let (status, body) = post(
        &app,
        "/api/orders",
        Some(&admin_token),
        Some(&json!({ "user_id": user_a, "amount": 100 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let order_id = body["id"].as_str().unwrap().to_string();

    // User A sees exactly their own order.
    let token_a = axum_helpers::login(&app, &email_a).await;
    let (status, body) = get(&app, "/api/users/me/orders", Some(&token_a)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["id"], order_id.as_str());

    // User B sees no orders.
    let token_b = axum_helpers::login(&app, &email_b).await;
    let (status, body) = get(&app, "/api/users/me/orders", Some(&token_b)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
}
