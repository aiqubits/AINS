//! 管理员用户管理模块集成测试
//!
//! 测试 CRUD 操作：list / create / get / update / delete

use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

mod common;
use common::{create_test_client, fixtures};

const ID1: &str = "1903487293645824000";
const ID2: &str = "1903487293645824001";
const ID3: &str = "1903487293645824002";
const BASE_TS: &str = "2024-01-15T08:00:00Z";
const UPDATED_TS: &str = "2024-06-09T12:00:00Z";

fn setup_admin_client(client: &client_api::Client) {
    client.set_token(fixtures::TEST_TOKEN);
}

// ──────────────────────────────────────────────
//  List users
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_list_users_empty() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [],
            "total": 0,
            "page": 1,
            "per_page": 10,
            "total_pages": 0,
        })))
        .mount(&mock_server)
        .await;

    let resp = client.list_users(1, 10).await.unwrap();
    assert!(resp.items.is_empty());
    assert_eq!(resp.total, 0);
    assert_eq!(resp.total_pages, 0);
}

#[tokio::test]
async fn test_list_users_with_data() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [
                fixtures::user_json(ID1, "user1@example.com", "Alice", "user", BASE_TS, BASE_TS, "default"),
                fixtures::user_json(ID2, "user2@example.com", "Bob", "admin", BASE_TS, BASE_TS, "default"),
            ],
            "total": 2,
            "page": 1,
            "per_page": 10,
            "total_pages": 1,
        })))
        .mount(&mock_server)
        .await;

    let resp = client.list_users(1, 10).await.unwrap();
    assert_eq!(resp.items.len(), 2);
    assert_eq!(resp.total, 2);
    assert_eq!(resp.items[0].name, "Alice");
    assert_eq!(resp.items[1].role, "admin");
}

#[tokio::test]
async fn test_list_users_pagination() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [fixtures::user_json(ID3, "u3@example.com", "Page2 User", "user", BASE_TS, BASE_TS, "default")],
            "total": 11,
            "page": 2,
            "per_page": 10,
            "total_pages": 2,
        })))
        .mount(&mock_server)
        .await;

    let resp = client.list_users(2, 10).await.unwrap();
    assert_eq!(resp.page, 2);
    assert_eq!(resp.total_pages, 2);
    assert_eq!(resp.items.len(), 1);
}

// ──────────────────────────────────────────────
//  Create user
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_create_user_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("POST"))
        .and(path("/api/users"))
        .and(body_json(serde_json::json!({
            "email": "new@example.com",
            "password": "SecurePass123!",
            "name": "New User",
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixtures::user_json(
                fixtures::TEST_USER_ID,
                "new@example.com",
                "New User",
                "user",
                BASE_TS,
                BASE_TS,
                "default",
            )),
        )
        .mount(&mock_server)
        .await;

    let user = client
        .create_user("new@example.com", "SecurePass123!", "New User", None, None)
        .await
        .unwrap();

    assert_eq!(user.email, "new@example.com");
    assert_eq!(user.name, "New User");
    assert_eq!(user.role, "user");
}

#[tokio::test]
async fn test_create_user_duplicate_email() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("POST"))
        .and(path("/api/users"))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "conflict",
            "message": "Email already registered",
        })))
        .mount(&mock_server)
        .await;

    let result = client
        .create_user(fixtures::TEST_EMAIL, "SecurePass123!", "Dup", None, None)
        .await;

    assert!(result.is_err());
}

// ──────────────────────────────────────────────
//  Get user
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_get_user_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    let id = fixtures::TEST_USER_ID.to_string();

    Mock::given(method("GET"))
        .and(path(format!("/api/users/{}", id)))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixtures::user_json(
                fixtures::TEST_USER_ID,
                fixtures::TEST_EMAIL,
                fixtures::TEST_NAME,
                "user",
                BASE_TS,
                BASE_TS,
                "default",
            )),
        )
        .mount(&mock_server)
        .await;

    let user = client.get_user(id).await.unwrap();
    assert_eq!(user.id.to_string(), fixtures::TEST_USER_ID);
    assert_eq!(user.email, fixtures::TEST_EMAIL);
}

#[tokio::test]
async fn test_get_user_not_found() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    let id = fixtures::TEST_USER_ID.to_string();

    Mock::given(method("GET"))
        .and(path(format!("/api/users/{}", id)))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "not_found",
            "message": "User not found",
        })))
        .mount(&mock_server)
        .await;

    let result = client.get_user(id).await;
    assert!(result.is_err());
}

// ──────────────────────────────────────────────
//  Update user
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_update_user_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    let id = fixtures::TEST_USER_ID.to_string();

    Mock::given(method("PUT"))
        .and(path(format!("/api/users/{}", id)))
        .and(body_json(serde_json::json!({
            "email": "updated@example.com",
            "name": "Updated Name",
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixtures::user_json(
                fixtures::TEST_USER_ID,
                "updated@example.com",
                "Updated Name",
                "user",
                BASE_TS,
                UPDATED_TS,
                "default",
            )),
        )
        .mount(&mock_server)
        .await;

    let user = client
        .update_user(
            id,
            Some("updated@example.com".into()),
            Some("Updated Name".into()),
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(user.email, "updated@example.com");
    assert_eq!(user.name, "Updated Name");
}

#[tokio::test]
async fn test_update_user_role_only() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    let id = fixtures::TEST_USER_ID.to_string();

    Mock::given(method("PUT"))
        .and(path(format!("/api/users/{}", id)))
        .and(body_json(serde_json::json!({
            "role": "admin",
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixtures::user_json(
                fixtures::TEST_USER_ID,
                fixtures::TEST_EMAIL,
                fixtures::TEST_NAME,
                "admin",
                BASE_TS,
                UPDATED_TS,
                "default",
            )),
        )
        .mount(&mock_server)
        .await;

    let user = client
        .update_user(id, None, None, Some("admin".into()), None)
        .await
        .unwrap();

    assert_eq!(user.role, "admin");
}

#[tokio::test]
async fn test_update_user_not_found() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    let id = fixtures::TEST_USER_ID.to_string();

    Mock::given(method("PUT"))
        .and(path(format!("/api/users/{}", id)))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "not_found",
            "message": "User not found",
        })))
        .mount(&mock_server)
        .await;

    let result = client
        .update_user(id, None, Some("New".into()), None, None)
        .await;
    assert!(result.is_err());
}

// ──────────────────────────────────────────────
//  Delete user
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_delete_user_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    let id = fixtures::TEST_USER_ID.to_string();

    Mock::given(method("DELETE"))
        .and(path(format!("/api/users/{}", id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": "User deleted successfully",
        })))
        .mount(&mock_server)
        .await;

    let resp = client.delete_user(id).await.unwrap();
    assert_eq!(resp.message, "User deleted successfully");
}

#[tokio::test]
async fn test_delete_user_not_found() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    let id = fixtures::TEST_USER_ID.to_string();

    Mock::given(method("DELETE"))
        .and(path(format!("/api/users/{}", id)))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "not_found",
            "message": "User not found",
        })))
        .mount(&mock_server)
        .await;

    let result = client.delete_user(id).await;
    assert!(result.is_err());
}

// ──────────────────────────────────────────────
//  Metering: list_usage
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_list_usage_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/usage"))
        .and(query_param("page", "1"))
        .and(query_param("per_page", "20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [{
                "id": 1,
                "user_id": 42,
                "tenant_id": "t1",
                "channel_id": "550e8400-e29b-41d4-a716-446655440000",
                "model": "gpt-4",
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "request_type": "chat",
                "created_at": "2026-06-01T00:00:00Z"
            }],
            "total": 1,
            "page": 1,
            "per_page": 20,
            "total_pages": 1
        })))
        .mount(&mock_server)
        .await;

    let resp = client.list_usage(1, 20, None).await.unwrap();
    assert_eq!(resp.items.len(), 1);
    assert_eq!(resp.items[0].model, "gpt-4");
    assert_eq!(resp.total, 1);
    assert_eq!(resp.page, 1);
}

#[tokio::test]
async fn test_list_usage_page_params_passthrough() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    // Client no longer rejects page=0 upfront; it passes through to the server.
    // Mock a successful response to verify the request reaches the server.
    Mock::given(method("GET"))
        .and(path("/api/usage"))
        .and(query_param("page", "0"))
        .and(query_param("per_page", "20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [],
            "total": 0,
            "page": 1,
            "per_page": 20,
            "total_pages": 0,
        })))
        .mount(&mock_server)
        .await;

    let result = client.list_usage(0, 20, None).await;
    assert!(
        result.is_ok(),
        "page=0 should be passed to server, not rejected by client"
    );

    Mock::given(method("GET"))
        .and(path("/api/usage"))
        .and(query_param("page", "1"))
        .and(query_param("per_page", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [],
            "total": 0,
            "page": 1,
            "per_page": 20,
            "total_pages": 0,
        })))
        .mount(&mock_server)
        .await;

    let result = client.list_usage(1, 0, None).await;
    assert!(
        result.is_ok(),
        "per_page=0 should be passed to server, not rejected by client"
    );
}

#[tokio::test]
async fn test_list_usage_unauthorized() {
    let (client, mock_server) = create_test_client().await;
    // No token set — client will send request without auth header

    Mock::given(method("GET"))
        .and(path("/api/usage"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "unauthorized",
            "message": "Invalid or expired token",
        })))
        .mount(&mock_server)
        .await;

    let result = client.list_usage(1, 20, None).await;
    assert!(matches!(
        result.unwrap_err(),
        client_api::ClientError::Other(401, _)
    ));
}

#[tokio::test]
async fn test_list_usage_with_filters() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/usage"))
        .and(query_param("page", "1"))
        .and(query_param("per_page", "20"))
        .and(query_param("model", "gpt-4"))
        .and(query_param("request_type", "chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [{
                "id": 10,
                "user_id": 42,
                "tenant_id": "t1",
                "channel_id": "550e8400-e29b-41d4-a716-446655440000",
                "model": "gpt-4",
                "prompt_tokens": 200,
                "completion_tokens": 100,
                "total_tokens": 300,
                "request_type": "chat",
                "created_at": "2026-06-15T00:00:00Z"
            }],
            "total": 1,
            "page": 1,
            "per_page": 20,
            "total_pages": 1
        })))
        .mount(&mock_server)
        .await;

    let filter = client_api::ListUsageFilter {
        user_id: None,
        channel_id: None,
        model: Some("gpt-4".to_string()),
        request_type: Some("chat".to_string()),
        date_from: None,
        date_to: None,
    };
    let resp = client.list_usage(1, 20, Some(&filter)).await.unwrap();
    assert_eq!(resp.items.len(), 1);
    assert_eq!(resp.items[0].model, "gpt-4");
}

#[tokio::test]
async fn test_get_usage_stats_with_filters() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/usage/stats"))
        .and(query_param("model", "gpt-4"))
        .and(query_param("date_from", "2026-01-01T00:00:00Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_requests": 50,
            "total_prompt_tokens": 2500,
            "total_completion_tokens": 1500,
            "total_tokens": 4000,
            "model_breakdown": [
                {
                    "model": "gpt-4",
                    "request_count": 50,
                    "total_tokens": 4000
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let filter = client_api::UsageStatsFilter {
        user_id: None,
        channel_id: None,
        model: Some("gpt-4".to_string()),
        request_type: None,
        date_from: Some("2026-01-01T00:00:00Z".to_string()),
        date_to: None,
    };
    let stats = client.get_usage_stats(Some(&filter)).await.unwrap();
    assert_eq!(stats.total_requests, 50);
    assert_eq!(stats.model_breakdown.len(), 1);
}

// ──────────────────────────────────────────────
//  Metering: get_usage_stats
// ──────────────────────────────────────────────

#[tokio::test]
async fn test_get_usage_stats_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/usage/stats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_requests": 100,
            "total_prompt_tokens": 5000,
            "total_completion_tokens": 3000,
            "total_tokens": 8000,
            "model_breakdown": [
                {
                    "model": "gpt-4",
                    "request_count": 80,
                    "total_tokens": 6000
                },
                {
                    "model": "claude-3",
                    "request_count": 20,
                    "total_tokens": 2000
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let stats = client.get_usage_stats(None).await.unwrap();
    assert_eq!(stats.total_requests, 100);
    assert_eq!(stats.total_tokens, 8000);
    assert_eq!(stats.model_breakdown.len(), 2);
}

#[tokio::test]
async fn test_get_usage_stats_empty_results() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/usage/stats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_requests": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "total_tokens": 0,
            "model_breakdown": []
        })))
        .mount(&mock_server)
        .await;

    let stats = client.get_usage_stats(None).await.unwrap();
    assert_eq!(stats.total_requests, 0);
    assert_eq!(stats.total_tokens, 0);
    assert!(stats.model_breakdown.is_empty());
}

#[tokio::test]
async fn test_list_usage_filter_by_user_id_and_channel() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    let channel_id = "550e8400-e29b-41d4-a716-446655440000";

    Mock::given(method("GET"))
        .and(path("/api/usage"))
        .and(query_param("user_id", "42"))
        .and(query_param("channel_id", channel_id))
        .and(query_param("date_from", "2026-01-01T00:00:00Z"))
        .and(query_param("date_to", "2026-01-31T23:59:59Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [],
            "total": 0,
            "page": 1,
            "per_page": 20,
            "total_pages": 0
        })))
        .mount(&mock_server)
        .await;

    let filter = client_api::ListUsageFilter {
        user_id: Some(42),
        channel_id: Some(channel_id.to_string()),
        model: None,
        request_type: None,
        date_from: Some("2026-01-01T00:00:00Z".to_string()),
        date_to: Some("2026-01-31T23:59:59Z".to_string()),
    };
    let resp = client.list_usage(1, 20, Some(&filter)).await.unwrap();
    assert_eq!(resp.total, 0);
}

#[tokio::test]
async fn test_get_usage_stats_filter_by_user_id() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/usage/stats"))
        .and(query_param("user_id", "42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_requests": 5,
            "total_prompt_tokens": 100,
            "total_completion_tokens": 50,
            "total_tokens": 150,
            "model_breakdown": []
        })))
        .mount(&mock_server)
        .await;

    let filter = client_api::UsageStatsFilter {
        user_id: Some(42),
        channel_id: None,
        model: None,
        request_type: None,
        date_from: None,
        date_to: None,
    };
    let stats = client.get_usage_stats(Some(&filter)).await.unwrap();
    assert_eq!(stats.total_requests, 5);
}

// ──────────────────────────────
//  Admin: Tenant management
// ──────────────────────────────

#[tokio::test]
async fn test_list_tenants_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/tenants"))
        .and(query_param("page", "1"))
        .and(query_param("per_page", "20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [
                { "id": "default", "name": "Default Tenant", "status": "active", "created_at": BASE_TS }
            ],
            "total": 1,
            "page": 1,
            "per_page": 20,
            "total_pages": 1
        })))
        .mount(&mock_server)
        .await;

    let resp = client.list_tenants(1, 20).await.unwrap();
    assert_eq!(resp.items.len(), 1);
    assert_eq!(resp.items[0].id, "default");
    assert_eq!(resp.items[0].status, "active");
}

#[tokio::test]
async fn test_create_tenant_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("POST"))
        .and(path("/api/tenants"))
        .and(body_json(serde_json::json!({ "name": "Acme" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "tnt_1", "name": "Acme", "status": "active", "created_at": BASE_TS
        })))
        .mount(&mock_server)
        .await;

    let resp = client.create_tenant("Acme").await.unwrap();
    assert_eq!(resp.name, "Acme");
    assert_eq!(resp.id, "tnt_1");
}

#[tokio::test]
async fn test_update_tenant_partial_body() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    // Only status provided → name must be omitted from the JSON body.
    Mock::given(method("PUT"))
        .and(path("/api/tenants/tnt_1"))
        .and(body_json(serde_json::json!({ "status": "suspended" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "tnt_1", "name": "Acme", "status": "suspended", "created_at": BASE_TS
        })))
        .mount(&mock_server)
        .await;

    let resp = client
        .update_tenant("tnt_1", None, Some("suspended".to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status, "suspended");
}

#[tokio::test]
async fn test_delete_tenant_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("DELETE"))
        .and(path("/api/tenants/tnt_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": "Tenant deleted successfully",
        })))
        .mount(&mock_server)
        .await;

    assert!(client.delete_tenant("tnt_1").await.is_ok());
}

#[tokio::test]
async fn test_delete_tenant_conflict() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("DELETE"))
        .and(path("/api/tenants/default"))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "conflict",
            "message": "Tenant has active users",
        })))
        .mount(&mock_server)
        .await;

    assert!(matches!(
        client.delete_tenant("default").await.unwrap_err(),
        client_api::ClientError::Other(409, _)
    ));
}

// ──────────────────────────────
//  Admin: Channel management
// ──────────────────────────────

const CHANNEL_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn channel_json() -> serde_json::Value {
    serde_json::json!({
        "id": CHANNEL_ID,
        "tenant_id": "default",
        "name": "OpenAI",
        "protocol_type": "openai",
        "models": ["gpt-4"],
        "capabilities": ["chat"],
        "base_url": "https://api.openai.com",
        "is_active": true,
        "weight": 10,
        "created_at": BASE_TS,
        "updated_at": UPDATED_TS
    })
}

#[tokio::test]
async fn test_list_channels_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("GET"))
        .and(path("/api/channels"))
        .and(query_param("page", "1"))
        .and(query_param("per_page", "20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [channel_json()],
            "total": 1,
            "page": 1,
            "per_page": 20,
            "total_pages": 1
        })))
        .mount(&mock_server)
        .await;

    let resp = client.list_channels(1, 20).await.unwrap();
    assert_eq!(resp.items.len(), 1);
    assert_eq!(resp.items[0].name, "OpenAI");
    assert!(resp.items[0].is_active);
}

#[tokio::test]
async fn test_create_channel_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    // tenant_id is None → omitted; is_active is a plain bool → always serialized.
    Mock::given(method("POST"))
        .and(path("/api/channels"))
        .and(body_json(serde_json::json!({
            "name": "OpenAI",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "api_key": "sk-secret",
            "base_url": "https://api.openai.com",
            "weight": 10,
            "is_active": true
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(channel_json()))
        .mount(&mock_server)
        .await;

    let input = client_api::CreateChannelRequest {
        name: "OpenAI".to_string(),
        protocol_type: "openai".to_string(),
        models: vec!["gpt-4".to_string()],
        capabilities: vec!["chat".to_string()],
        api_key: "sk-secret".to_string(),
        base_url: "https://api.openai.com".to_string(),
        weight: 10,
        is_active: true,
        tenant_id: None,
    };
    let resp = client.create_channel(input).await.unwrap();
    assert_eq!(resp.name, "OpenAI");
}

#[tokio::test]
async fn test_update_channel_partial_body() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    // Only weight + is_active provided → all other optional fields omitted.
    Mock::given(method("PUT"))
        .and(path(format!("/api/channels/{}", CHANNEL_ID)))
        .and(body_json(
            serde_json::json!({ "is_active": false, "weight": 5 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(channel_json()))
        .mount(&mock_server)
        .await;

    let input = client_api::UpdateChannelRequest {
        name: None,
        protocol_type: None,
        models: None,
        capabilities: None,
        api_key: None,
        base_url: None,
        is_active: Some(false),
        weight: Some(5),
        tenant_id: None,
    };
    let resp = client.update_channel(CHANNEL_ID, input).await.unwrap();
    assert_eq!(resp.id, CHANNEL_ID);
}

#[tokio::test]
async fn test_disable_channel_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("PUT"))
        .and(path(format!("/api/channels/{}", CHANNEL_ID)))
        .and(body_json(serde_json::json!({"is_active": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": CHANNEL_ID,
            "name": "OpenAI",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "base_url": "https://api.openai.com",
            "is_active": false,
            "weight": 1,
            "created_at": BASE_TS,
            "updated_at": BASE_TS,
        })))
        .mount(&mock_server)
        .await;

    assert!(client.disable_channel(CHANNEL_ID).await.is_ok());
}

#[tokio::test]
async fn test_disable_channel_idempotent() {
    // Disabling an already-disabled channel must still resolve to Ok(()):
    // the client only cares that the PUT succeeds, not the prior state.
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("PUT"))
        .and(path(format!("/api/channels/{}", CHANNEL_ID)))
        .and(body_json(serde_json::json!({"is_active": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": CHANNEL_ID,
            "tenant_id": "default",
            "name": "OpenAI",
            "protocol_type": "openai",
            "models": ["gpt-4"],
            "capabilities": ["chat"],
            "base_url": "https://api.openai.com",
            "is_active": false,
            "weight": 1,
            "created_at": BASE_TS,
            "updated_at": UPDATED_TS,
        })))
        .expect(2)
        .mount(&mock_server)
        .await;

    assert!(client.disable_channel(CHANNEL_ID).await.is_ok());
    assert!(client.disable_channel(CHANNEL_ID).await.is_ok());
}

#[tokio::test]
async fn test_delete_channel_success() {
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("DELETE"))
        .and(path(format!("/api/channels/{}", CHANNEL_ID)))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": "Channel deleted successfully",
        })))
        .mount(&mock_server)
        .await;

    assert!(client.delete_channel(CHANNEL_ID).await.is_ok());
}

#[tokio::test]
async fn test_delete_channel_conflict() {
    // A channel with associated token_usage records cannot be deleted; the
    // server returns 409 Conflict, surfaced as ClientError::Other(409, _).
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    Mock::given(method("DELETE"))
        .and(path(format!("/api/channels/{}", CHANNEL_ID)))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "conflict",
            "message": "Channel has usage records",
        })))
        .mount(&mock_server)
        .await;

    assert!(matches!(
        client.delete_channel(CHANNEL_ID).await.unwrap_err(),
        client_api::ClientError::Other(409, _)
    ));
}

// ──────────────────────────────
//  Client-side page validation
// ──────────────────────────────

#[tokio::test]
async fn test_list_channels_rejects_zero_page() {
    // Unlike list_usage (which passes page/per_page through to the server),
    // list_channels validates client-side and fails fast with Config.
    let (client, _mock_server) = create_test_client().await;
    setup_admin_client(&client);

    assert!(matches!(
        client.list_channels(0, 20).await.unwrap_err(),
        client_api::ClientError::Config(_)
    ));
    assert!(matches!(
        client.list_channels(1, 0).await.unwrap_err(),
        client_api::ClientError::Config(_)
    ));
}

#[tokio::test]
async fn test_list_tenants_rejects_zero_page() {
    let (client, _mock_server) = create_test_client().await;
    setup_admin_client(&client);

    assert!(matches!(
        client.list_tenants(0, 20).await.unwrap_err(),
        client_api::ClientError::Config(_)
    ));
    assert!(matches!(
        client.list_tenants(1, 0).await.unwrap_err(),
        client_api::ClientError::Config(_)
    ));
}

// ──────────────────────────────
//  UserResponse deserialization
// ──────────────────────────────

#[tokio::test]
async fn test_user_response_missing_tenant_id_defaults_empty() {
    // UserResponse.tenant_id uses #[serde(default)] for backward compatibility:
    // if an older server omits the field, deserialization must still succeed
    // and yield an empty string (never a hard failure).
    let (client, mock_server) = create_test_client().await;
    setup_admin_client(&client);

    // Build the body inline WITHOUT tenant_id (the fixture always includes it).
    Mock::given(method("GET"))
        .and(path(format!("/api/users/{}", ID1)))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": ID1,
            "email": "a@example.com",
            "name": "Alice",
            "role": "user",
            "created_at": BASE_TS,
            "updated_at": UPDATED_TS,
        })))
        .mount(&mock_server)
        .await;

    let user = client.get_user(ID1.to_string()).await.unwrap();
    assert_eq!(user.tenant_id, "");
    assert!(user.tenant_name.is_none());
}
