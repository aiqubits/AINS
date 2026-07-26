//! 套餐与支付订单模块集成测试（wiremock）
//!
//! 覆盖新增 Client 方法的关键行为：purchase_plan 的错误映射
//! （insufficient_balance / no_active_plan 场景所依赖的 400/404 传递）、
//! list_orders 过滤参数序列化、以及套餐 CRUD 的请求/响应契约。

use client_api::{ClientError, CreatePlanRequest, ListOrdersFilter, UpdateOrderRequest};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

mod common;
use common::{create_test_client, fixtures};

const PLAN_ID: &str = "1903487293645824100";
const ORDER_ID: &str = "1903487293645824200";
const USER_ID: &str = "1903487293645824300";
const BASE_TS: &str = "2024-01-15T08:00:00Z";

fn plan_json(id: &str, name: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "tenant_id": "default",
        "name": name,
        "description": "test plan",
        "price": 40_000_000_000i64,
        "total_calls": 100,
        "validity_days": 30,
        "status": status,
        "created_at": BASE_TS,
        "updated_at": BASE_TS,
    })
}

fn order_json(id: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "user_id": USER_ID,
        "tenant_id": "default",
        "user_email": "buyer@example.com",
        "plan_id": PLAN_ID,
        "plan_name": "Starter",
        "amount": 40_000_000_000i64,
        "status": status,
        "payment_method": "balance",
        "created_at": BASE_TS,
    })
}

// ──────────────────────────────────────────────
//  Plans
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_list_plans_sends_tenant_filter() {
    let (client, mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    Mock::given(method("GET"))
        .and(path("/api/plans"))
        .and(query_param("page", "1"))
        .and(query_param("per_page", "20"))
        .and(query_param("tenant_id", "tenant-x"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [plan_json(PLAN_ID, "Starter", "active")],
            "total": 1,
            "page": 1,
            "per_page": 20,
            "total_pages": 1,
        })))
        .mount(&mock_server)
        .await;

    let resp = client
        .list_plans(1, 20, Some("tenant-x".to_string()))
        .await
        .unwrap();
    assert_eq!(resp.items.len(), 1);
    assert_eq!(resp.items[0].name, "Starter");
    assert_eq!(resp.items[0].price, 40_000_000_000i64);
}

#[tokio::test]
async fn test_create_plan_serializes_optional_fields() {
    let (client, mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    // tenant_id: None must be omitted from the JSON body entirely.
    Mock::given(method("POST"))
        .and(path("/api/plans"))
        .and(body_json(serde_json::json!({
            "name": "Starter",
            "price": 40_000_000_000i64,
            "total_calls": 100,
            "validity_days": 30,
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(plan_json(PLAN_ID, "Starter", "active")),
        )
        .mount(&mock_server)
        .await;

    let resp = client
        .create_plan(CreatePlanRequest {
            tenant_id: None,
            name: "Starter".to_string(),
            description: None,
            price: 40_000_000_000,
            total_calls: 100,
            validity_days: 30,
            status: None,
        })
        .await
        .unwrap();
    assert_eq!(resp.id, PLAN_ID);
}

#[tokio::test]
async fn test_purchase_plan_success() {
    let (client, mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    Mock::given(method("POST"))
        .and(path(format!("/api/plans/{}/purchase", PLAN_ID)))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "order": order_json(ORDER_ID, "paid"),
            "user_plan": {
                "id": "1903487293645824400",
                "user_id": USER_ID,
                "plan_id": PLAN_ID,
                "plan_name": "Starter",
                "total_calls": 100,
                "remaining_calls": 100,
                "expires_at": "2024-02-14T08:00:00Z",
                "source": "purchase",
                "created_at": BASE_TS,
                "status": "active",
            },
            "balance": 60_000_000_000i64,
            "display_balance": 6.0,
            "message": "Plan purchased successfully",
        })))
        .mount(&mock_server)
        .await;

    let resp = client.purchase_plan(PLAN_ID).await.unwrap();
    assert_eq!(resp.order.status, "paid");
    assert_eq!(resp.user_plan.remaining_calls, 100);
    assert_eq!(resp.balance, 60_000_000_000i64);
}

#[tokio::test]
async fn test_purchase_plan_insufficient_balance_maps_to_other_400() {
    let (client, mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    // The raw body must be passed through so humanize_error can read the
    // `error` code — assert both the status and the preserved payload.
    Mock::given(method("POST"))
        .and(path(format!("/api/plans/{}/purchase", PLAN_ID)))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "insufficient_balance",
            "message": "Insufficient balance to purchase this plan",
        })))
        .mount(&mock_server)
        .await;

    let err = client.purchase_plan(PLAN_ID).await.unwrap_err();
    match err {
        ClientError::Other(400, body) => {
            assert!(body.contains("insufficient_balance"), "body: {body}");
        }
        other => panic!("expected Other(400, ..), got: {other:?}"),
    }
}

#[tokio::test]
async fn test_purchase_plan_not_found() {
    let (client, mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    Mock::given(method("POST"))
        .and(path(format!("/api/plans/{}/purchase", PLAN_ID)))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "not_found",
            "message": "Plan not found",
        })))
        .mount(&mock_server)
        .await;

    let err = client.purchase_plan(PLAN_ID).await.unwrap_err();
    assert!(matches!(err, ClientError::Other(404, _)), "got: {err:?}");
}

#[tokio::test]
async fn test_list_available_and_my_plans() {
    let (client, mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    Mock::given(method("GET"))
        .and(path("/api/plans/available"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [plan_json(PLAN_ID, "Starter", "active")],
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/users/me/plans"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [],
        })))
        .mount(&mock_server)
        .await;

    let avail = client.list_available_plans().await.unwrap();
    assert_eq!(avail.items.len(), 1);
    let mine = client.list_my_plans().await.unwrap();
    assert!(mine.items.is_empty());
}

// ──────────────────────────────────────────────
//  Payment orders
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_list_orders_serializes_filters() {
    let (client, mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    Mock::given(method("GET"))
        .and(path("/api/orders"))
        .and(query_param("page", "2"))
        .and(query_param("per_page", "50"))
        .and(query_param("tenant_id", "tenant-x"))
        .and(query_param("user_id", USER_ID))
        .and(query_param("status", "paid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [order_json(ORDER_ID, "paid")],
            "total": 1,
            "page": 2,
            "per_page": 50,
            "total_pages": 1,
        })))
        .mount(&mock_server)
        .await;

    let filter = ListOrdersFilter {
        tenant_id: Some("tenant-x".to_string()),
        user_id: Some(USER_ID.to_string()),
        status: Some("paid".to_string()),
    };
    let resp = client.list_orders(2, 50, Some(&filter)).await.unwrap();
    assert_eq!(resp.total, 1);
    assert_eq!(resp.items[0].user_email, "buyer@example.com");
    assert_eq!(resp.items[0].paid_at, None, "absent paid_at deserializes");
}

#[tokio::test]
async fn test_update_order_omits_unset_fields() {
    let (client, mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    // Only `status` may appear in the body; None fields must be omitted so
    // the server treats them as "unchanged".
    Mock::given(method("PUT"))
        .and(path(format!("/api/orders/{}", ORDER_ID)))
        .and(body_json(serde_json::json!({ "status": "refunded" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(order_json(ORDER_ID, "refunded")))
        .mount(&mock_server)
        .await;

    let resp = client
        .update_order(
            ORDER_ID,
            UpdateOrderRequest {
                status: Some("refunded".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(resp.status, "refunded");
}

#[tokio::test]
async fn test_pagination_zero_rejected_client_side() {
    let (client, _mock_server) = create_test_client().await;
    client.set_token(fixtures::TEST_TOKEN);

    assert!(matches!(
        client.list_plans(0, 20, None).await.unwrap_err(),
        ClientError::Config(_)
    ));
    assert!(matches!(
        client.list_orders(1, 0, None).await.unwrap_err(),
        ClientError::Config(_)
    ));
    assert!(matches!(
        client.list_my_orders(0, 20).await.unwrap_err(),
        ClientError::Config(_)
    ));
}
