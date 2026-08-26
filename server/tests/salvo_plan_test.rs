#![cfg(feature = "ains-salvo")]

//! Salvo-mode smoke tests for plan management & payment orders.
//!
//! The axum suite (axum_plan_test.rs / axum_payment_order_test.rs) carries the
//! full behavioral coverage; this file guards the framework-specific risks:
//! route registration order (`/plans/available` vs `/plans/{id}`, `/users/me/*`
//! vs `/users/{id}/*`), the admin guard hoop, and the /api/ai/response
//! plan-quota gate under the salvo adapter.

use serde_json::json;

mod common;
use common::salvo;

#[tokio::test]
async fn test_salvo_plan_routes_and_quota_gate() {
    let server = salvo::create_test_server().await;

    // ── System creates a plan in the default tenant ──
    let sys_email = common::unique_email("sv_plan_sys");
    let sys_token = salvo::create_system_and_login(&server, &sys_email).await;
    let (status, plan) = salvo::post(
        &server,
        "/api/plans",
        Some(&sys_token),
        Some(&json!({
            "tenant_id": "default",
            "name": format!("Salvo Plan {}", common::unique_table_name("sv")),
            "price": 40_000_000_000i64,
            "total_calls": 2,
            "validity_days": 30,
            "purchase_limit": 1,
        })),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "salvo plan creation should succeed: {plan}"
    );
    assert_eq!(plan["purchase_limit"], 1);
    let plan_id = plan["id"].as_str().unwrap().to_string();

    // ── Regular user in the default tenant ──
    let user_email = common::unique_email("sv_plan_user");
    let user_token = salvo::register_and_login(&server, &user_email).await;

    // Admin guard hoop: user role must be rejected from admin routes.
    let (status, _) = salvo::get(&server, "/api/plans", Some(&user_token)).await;
    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "user must not access admin plan listing under salvo"
    );

    // Route order: /plans/available must NOT be swallowed by /plans/{id}.
    let (status, body) = salvo::get(&server, "/api/plans/available", Some(&user_token)).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "GET /plans/available should match the self route: {body}"
    );
    let visible = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == plan_id.as_str())
        .expect("created plan should be purchasable");
    assert_eq!(visible["purchases_used"], 0, "body: {body}");

    // Route order: /users/me/plans must NOT be swallowed by /users/{id}/plans
    // (the latter is admin-guarded and would 403 for a user role).
    let (status, body) = salvo::get(&server, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "me/plans: {body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 0);

    // Quota gate: user without a plan is rejected with no_active_plan.
    let (status, body) = salvo::post(
        &server,
        "/api/ai/response",
        Some(&user_token),
        Some(&json!({ "input": "hello" })),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["code"], "no_active_plan");

    // ── Fund the user, purchase via /plans/{id}/purchase (self route) ──
    let (status, me) = salvo::get(&server, "/api/users/me", Some(&user_token)).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let user_id = me["id"].as_str().unwrap().to_string();
    let (status, _) = salvo::put(
        &server,
        &format!("/api/users/{}/balance", user_id),
        Some(&sys_token),
        Some(&json!({ "balance": 100_000_000_000i64 })),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let (status, body) = salvo::post(
        &server,
        &format!("/api/plans/{}/purchase", plan_id),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "purchase should succeed under salvo: {body}"
    );
    assert_eq!(body["order"]["status"], "paid");
    assert_eq!(body["balance"], 60_000_000_000i64);

    let (status, available) = salvo::get(&server, "/api/plans/available", Some(&user_token)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {available}");
    let limited = available["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == plan_id.as_str())
        .expect("limited plan remains visible");
    assert_eq!(limited["purchases_used"], 1);

    // The nullable update contract and terminal purchase-limit conflict must
    // behave identically through Salvo's request adapter.
    let (status, body) = salvo::post(
        &server,
        &format!("/api/plans/{}/purchase", plan_id),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"], "purchase_limit_reached");

    let (status, body) = salvo::put(
        &server,
        &format!("/api/plans/{}", plan_id),
        Some(&sys_token),
        Some(&json!({ "purchase_limit": null })),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "clear limit: {body}");
    assert!(body["purchase_limit"].is_null());

    let (status, body) = salvo::post(
        &server,
        &format!("/api/plans/{}/purchase", plan_id),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "purchase after clearing limit: {body}"
    );
    assert_eq!(body["balance"], 20_000_000_000i64);

    // me/plans now shows the active instance; me/orders shows the purchase.
    let (status, body) = salvo::get(&server, "/api/users/me/plans", Some(&user_token)).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["items"][0]["status"], "active");
    assert_eq!(body["items"][0]["remaining_calls"], 2);

    let (status, body) = salvo::get(&server, "/api/users/me/orders", Some(&user_token)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "me/orders: {body}");
    assert_eq!(body["total"], 2);
    assert_eq!(body["items"][0]["user_email"], user_email.as_str());

    // Free-plan parity: Salvo must deserialize a zero price, persist it, and
    // execute the shared transactional purchase path without changing balance.
    let (status, free_plan) = salvo::post(
        &server,
        "/api/plans",
        Some(&sys_token),
        Some(&json!({
            "tenant_id": "default",
            "name": format!("Salvo Free Plan {}", common::unique_table_name("sv_free")),
            "price": 0,
            "total_calls": 1,
            "validity_days": 1,
        })),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "salvo free-plan creation should succeed: {free_plan}"
    );
    assert_eq!(free_plan["price"], 0);
    let free_plan_id = free_plan["id"].as_str().unwrap();

    let (status, free_purchase) = salvo::post(
        &server,
        &format!("/api/plans/{free_plan_id}/purchase"),
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "salvo free-plan purchase should succeed: {free_purchase}"
    );
    assert_eq!(free_purchase["order"]["amount"], 0);
    assert_eq!(free_purchase["balance"], 20_000_000_000i64);

    // Admin order listing works and the user is still barred from it.
    let (status, _) = salvo::get(&server, "/api/orders", Some(&sys_token)).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let (status, _) = salvo::get(&server, "/api/orders", Some(&user_token)).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);

    // ── Order CRUD parity under salvo (create → get → paid stamp → delete) ──
    let (status, order) = salvo::post(
        &server,
        "/api/orders",
        Some(&sys_token),
        Some(&json!({
            "user_id": user_id,
            "amount": 123,
            "status": "pending",
            "payment_method": "wechat",
            "external_txn_id": "wx_salvo",
        })),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "order create: {order}");
    let order_id = order["id"].as_str().unwrap().to_string();

    let (status, body) = salvo::get(
        &server,
        &format!("/api/orders/{}", order_id),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["status"], "pending");

    let (status, body) = salvo::put(
        &server,
        &format!("/api/orders/{}", order_id),
        Some(&sys_token),
        Some(&json!({ "status": "paid" })),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "order update: {body}");
    assert!(body["paid_at"].is_string(), "paid_at stamped: {body}");

    // Order deletion is system-only — a tenant admin gets 403 under the
    // salvo adapter too (the role check lives in the shared handler).
    let admin_email = common::unique_email("sv_ord_adm");
    let (status, _) = salvo::post(
        &server,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": admin_email,
            "password": common::DEFAULT_TEST_PASSWORD,
            "name": "Salvo Order Admin",
            "role": "admin",
        })),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let (status, body) = salvo::post_json(
        &server,
        "/api/public/auth/login",
        &json!({ "email": admin_email, "password": common::DEFAULT_TEST_PASSWORD }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let admin_token = body["token"].as_str().unwrap().to_string();
    let (status, _) = salvo::delete(
        &server,
        &format!("/api/orders/{}", order_id),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);

    let (status, _) = salvo::delete(
        &server,
        &format!("/api/orders/{}", order_id),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let (status, _) = salvo::get(
        &server,
        &format!("/api/orders/{}", order_id),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}
