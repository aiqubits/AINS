#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for multi-tenant CRUD operations.
//!
//! These tests require running PostgreSQL and Redis instances.
//! Start services before running:
//! - PostgreSQL: default port 5432
//! - Redis: default port 6379
//!
//! Run: cargo test --test axum_tenant_test

use ains_axum::{Body, BodyExt, Method, Router, StatusCode};
use serde_json::{Value, json};

mod common;
use common::axum as axum_helpers;

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

async fn delete(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut headers = Vec::new();
    if let Some(t) = token {
        headers.push(("authorization", format!("Bearer {}", t)));
    }
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let resp = axum_helpers::send_request(app, Method::DELETE, uri, headers, Body::empty()).await;
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json_body = if bytes.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json_body)
}

// ── Tenant CRUD tests ──────────────────────────────────────────

#[tokio::test]
async fn test_tenant_list_as_regular_user_returns_403() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let token = axum_helpers::register_and_login(&app, &common::unique_email("t_list_user")).await;

    let (status, _body) = get(&app, "/api/tenants", Some(&token)).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_tenant_list_as_admin_returns_own_tenant() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_list_admin");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    let (status, body) = get(&app, "/api/tenants", Some(&token)).await;

    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items should be an array");
    assert_eq!(items.len(), 1, "admin should only see their own tenant");
    assert_eq!(items[0]["id"], "default");
    assert!(
        !items[0]["name"].as_str().unwrap_or("").is_empty(),
        "tenant name should not be empty"
    );
    assert_eq!(items[0]["status"], "active");
}

#[tokio::test]
async fn test_tenant_list_as_system_returns_all_tenants() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_list_sys");
    let token = axum_helpers::create_system_and_login(&app, &email).await;

    let (status, body) = get(&app, "/api/tenants", Some(&token)).await;

    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items should be an array");
    assert!(!items.is_empty(), "system should see all tenants");
    let default = items.iter().find(|t| t["id"] == "default");
    assert!(default.is_some(), "default tenant should be in list");
}

#[tokio::test]
async fn test_tenant_create_as_system_succeeds() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_create_sys");
    let token = axum_helpers::create_system_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&token),
        Some(&json!({
            "name": "New Test Tenant"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["id"].is_string());
    assert_eq!(body["name"], "New Test Tenant");
    assert_eq!(body["status"], "active");
    assert_ne!(
        body["id"], "default",
        "new tenant should have a UUID, not 'default'"
    );
}

#[tokio::test]
async fn test_tenant_create_as_admin_returns_403() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_create_admin");
    let token = axum_helpers::create_admin_and_login(&app, &email).await;

    let (status, _body) = post(
        &app,
        "/api/tenants",
        Some(&token),
        Some(&json!({
            "name": "Should Fail"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_tenant_update_name_as_system() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_upd_name");
    let token = axum_helpers::create_system_and_login(&app, &email).await;

    let (status, body) = put(
        &app,
        "/api/tenants/default",
        Some(&token),
        Some(&json!({
            "name": "Updated Default Tenant"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Updated Default Tenant");
    assert_eq!(body["id"], "default");
}

#[tokio::test]
async fn test_tenant_cannot_disable_default_tenant() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_disable_def");
    let token = axum_helpers::create_system_and_login(&app, &email).await;

    let (status, _body) = put(
        &app,
        "/api/tenants/default",
        Some(&token),
        Some(&json!({
            "status": "disabled"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_tenant_disable_and_re_enable_non_default() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_toggle");
    let token = axum_helpers::create_system_and_login(&app, &email).await;

    // Create a new tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&token),
        Some(&json!({
            "name": "Toggle Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    // Disable it
    let (status, body) = put(
        &app,
        &format!("/api/tenants/{}", tenant_id),
        Some(&token),
        Some(&json!({
            "status": "disabled"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "disabled");

    // Re-enable it
    let (status, body) = put(
        &app,
        &format!("/api/tenants/{}", tenant_id),
        Some(&token),
        Some(&json!({
            "status": "active"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "active");
}

#[tokio::test]
async fn test_tenant_delete_empty_tenant_succeeds() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_delete_ok");
    let token = axum_helpers::create_system_and_login(&app, &email).await;

    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&token),
        Some(&json!({
            "name": "Delete Me"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    let (status, _body) = delete(&app, &format!("/api/tenants/{}", tenant_id), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_tenant_delete_default_tenant_returns_400() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_delete_def");
    let token = axum_helpers::create_system_and_login(&app, &email).await;

    let (status, body) = delete(&app, "/api/tenants/default", Some(&token)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("Cannot delete the default tenant"),
        "should indicate default tenant cannot be deleted; got: {}",
        msg
    );
}

#[tokio::test]
async fn test_tenant_delete_with_users_returns_409() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let email = common::unique_email("t_delete_has_users");
    let token = axum_helpers::create_system_and_login(&app, &email).await;

    // Create a second tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&token),
        Some(&json!({
            "name": "Occupied Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    // Create a user in that tenant via system API
    let user_email = common::unique_email("occupant");
    let (status, _body) = post(
        &app,
        "/api/users",
        Some(&token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Occupant User",
            "tenant_id": tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Delete should fail because tenant has a user
    let (status, body) = delete(&app, &format!("/api/tenants/{}", tenant_id), Some(&token)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let msg = body["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("user(s)") && msg.contains("Remove them first"),
        "should indicate tenant still contains users; got: {}",
        msg
    );
}

#[tokio::test]
async fn test_move_user_tenant_as_system() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("t_move_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Target Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let target_tenant_id = body["id"].as_str().unwrap().to_string();

    // Create a regular user
    let user_email = common::unique_email("movable_user");
    let user_token = axum_helpers::register_and_login(&app, &user_email).await;

    // Get user ID from the token
    let config = common::load_test_config();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    validation.set_issuer(&["ains-server"]);
    validation.set_audience(&["ains"]);
    let token_data = jsonwebtoken::decode::<ains_runtime::JwtClaims>(
        &user_token,
        &jsonwebtoken::DecodingKey::from_secret(config.jwt_secret.as_bytes()),
        &validation,
    )
    .expect("Failed to decode token");
    let user_id = token_data.claims.sub;

    // System moves the user to the target tenant
    let (status, body) = put(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&sys_token),
        Some(&json!({
            "tenant_id": target_tenant_id,
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["tenant_id"], target_tenant_id,
        "user should be moved to target tenant"
    );
}

#[tokio::test]
async fn test_move_user_tenant_as_admin_within_own_tenant() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let admin_email = common::unique_email("t_move_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    // Create a user under the admin's tenant (default)
    let user_email = common::unique_email("admin_movable");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&admin_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Movable User",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id = body["id"]
        .as_str()
        .unwrap_or_else(|| body["user_id"].as_str().unwrap_or(""));

    // Admin moves user within the same tenant
    let (status, body) = put(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&admin_token),
        Some(&json!({
            "tenant_id": "default",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant_id"], "default");
}

#[tokio::test]
async fn test_move_user_tenant_admin_cannot_move_to_other_tenant() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("t_move_cross_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;
    let admin_email = common::unique_email("t_move_cross_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

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
    let other_tenant = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("cross_target");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&admin_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Cross Target",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id = body["id"]
        .as_str()
        .unwrap_or_else(|| body["user_id"].as_str().unwrap_or(""));

    let (status, _body) = put(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&admin_token),
        Some(&json!({
            "tenant_id": other_tenant,
        })),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_move_user_to_disabled_tenant_returns_400() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("t_move_disabled");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a tenant, then disable it
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "Disabled Target"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    let _ = put(
        &app,
        &format!("/api/tenants/{}", tenant_id),
        Some(&sys_token),
        Some(&json!({
            "status": "disabled"
        })),
    )
    .await;

    // Create a user
    let user_email = common::unique_email("disabled_target");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Target User",
            "tenant_id": "default",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id = body["id"]
        .as_str()
        .unwrap_or_else(|| body["user_id"].as_str().unwrap_or(""));

    // Move to disabled tenant should fail
    let (status, _body) = put(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&sys_token),
        Some(&json!({
            "tenant_id": tenant_id,
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "moving user to disabled tenant should fail"
    );
}

#[tokio::test]
async fn test_move_user_to_nonexistent_tenant_returns_400() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("t_move_nonexist");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a user in the default tenant
    let user_email = common::unique_email("nonexist_target");
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": user_email,
            "password": "Password123!",
            "name": "Target User",
            "tenant_id": "default",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id = body["id"]
        .as_str()
        .unwrap_or_else(|| body["user_id"].as_str().unwrap_or(""));

    // Try to move the user to a tenant_id that does not exist
    let nonexistent_tenant = "00000000-0000-0000-0000-000000000000";
    let (status, _body) = put(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&sys_token),
        Some(&json!({
            "tenant_id": nonexistent_tenant,
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "moving user to a non-existent tenant should return 400"
    );
}

// ── Disabled tenant user creation ─────────────────────────────

#[tokio::test]
async fn test_create_user_in_disabled_tenant_rejected() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("cu_dis_tnt");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a new tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({"name": "Temp Tenant"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_id = body["id"].as_str().unwrap().to_string();

    // Disable the tenant
    let (status, _) = put(
        &app,
        &format!("/api/tenants/{}", tenant_id),
        Some(&sys_token),
        Some(&json!({"status": "disabled"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // System tries to create a user in the disabled tenant → must fail
    let (status, body) = post(
        &app,
        "/api/users",
        Some(&sys_token),
        Some(&json!({
            "email": common::unique_email("disabled_tenant_user"),
            "password": "Password123!",
            "name": "Should Fail",
            "tenant_id": tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("Tenant does not exist") || msg.contains("disabled"),
        "should mention disabled tenant; got: {}",
        msg
    );
}

// ── JWT invalidation after tenant move ──────────────────────────

#[tokio::test]
async fn test_move_user_tenant_invalidates_old_jwt() {
    let (app, _state) = axum_helpers::create_app_and_state().await;
    let sys_email = common::unique_email("jwt_move_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant
    let (status, body) = post(
        &app,
        "/api/tenants",
        Some(&sys_token),
        Some(&json!({
            "name": "JWT Invalidation Tenant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let target_tenant_id = body["id"].as_str().unwrap().to_string();

    // Register a user in the default tenant
    let user_email = common::unique_email("jwt_move_user");
    let user_token = axum_helpers::register_and_login(&app, &user_email).await;

    // Decode the JWT to verify initial tenant_id
    let config = common::load_test_config();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    validation.set_issuer(&["ains-server"]);
    validation.set_audience(&["ains"]);
    let token_data = jsonwebtoken::decode::<ains_runtime::JwtClaims>(
        &user_token,
        &jsonwebtoken::DecodingKey::from_secret(config.jwt_secret.as_bytes()),
        &validation,
    )
    .expect("Failed to decode initial token");
    let user_id = token_data.claims.sub.clone();
    assert_eq!(
        token_data.claims.tenant_id, "default",
        "initial tenant should be default"
    );

    // System moves the user to the new tenant — increments token_version
    let (status, body) = put(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&sys_token),
        Some(&json!({
            "tenant_id": target_tenant_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant_id"], target_tenant_id, "user should be moved");

    // Old JWT should now be invalid (token_version mismatch)
    // Use the unified AI response endpoint as a simple auth-required check.
    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&user_token),
        Some(&json!({
            "input": "hello"
        })),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "old JWT should be rejected after tenant move (token_version incremented)"
    );

    // Login again to get a new JWT with updated token_version
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
    let new_token = body["token"].as_str().unwrap().to_string();

    // New JWT should have the updated tenant_id
    let new_token_data = jsonwebtoken::decode::<ains_runtime::JwtClaims>(
        &new_token,
        &jsonwebtoken::DecodingKey::from_secret(config.jwt_secret.as_bytes()),
        &validation,
    )
    .expect("Failed to decode new token");
    assert_eq!(
        new_token_data.claims.tenant_id, target_tenant_id,
        "new JWT should reflect the moved tenant_id"
    );

    // New JWT should be accepted
    let (status, _body) = post(
        &app,
        "/api/ai/response",
        Some(&new_token),
        Some(&json!({
            "input": "hello"
        })),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "new JWT should be accepted (got status: {})",
        status.as_u16()
    );
}
