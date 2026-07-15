#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for cross-tenant user operation isolation.
//!
//! Verifies that admins cannot get/update/delete users belonging to other
//! tenants (returns NotFound, not Forbidden, to prevent user enumeration).
//! System role can operate on any tenant.
//!
//! These tests require running PostgreSQL and Redis instances.
//! Run: cargo test --test axum_cross_tenant_user_test

use ains_axum::{Body, BodyExt, Method, Router, StatusCode};
use serde_json::{Value, json};

mod common;
use common::axum as axum_helpers;

async fn body_to_json(response: ains_axum::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get_json(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let auth: String;
    let mut headers = Vec::new();
    if let Some(t) = token {
        auth = format!("Bearer {}", t);
        headers.push(("authorization", auth.as_str()));
    }
    let h: Vec<(&str, &str)> = headers;
    let resp = axum_helpers::send_request(app, Method::GET, uri, h, Body::empty()).await;
    (resp.status(), body_to_json(resp).await)
}

async fn put_json(
    app: &Router,
    uri: &str,
    token: Option<&str>,
    body_val: Option<&Value>,
) -> (StatusCode, Value) {
    let auth: String;
    let mut headers = vec![("content-type", "application/json")];
    if let Some(t) = token {
        auth = format!("Bearer {}", t);
        headers.push(("authorization", auth.as_str()));
    }
    let h: Vec<(&str, &str)> = headers;
    let body_str = body_val
        .map(|b| serde_json::to_string(b).unwrap())
        .unwrap_or_default();
    let resp = axum_helpers::send_request(app, Method::PUT, uri, h, Body::from(body_str)).await;
    (resp.status(), body_to_json(resp).await)
}

async fn delete_json(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let auth: String;
    let mut headers = Vec::new();
    if let Some(t) = token {
        auth = format!("Bearer {}", t);
        headers.push(("authorization", auth.as_str()));
    }
    let h: Vec<(&str, &str)> = headers;
    let resp = axum_helpers::send_request(app, Method::DELETE, uri, h, Body::empty()).await;
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json_body = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json_body)
}

async fn post_json(
    app: &Router,
    uri: &str,
    token: Option<&str>,
    body_val: &Value,
) -> (StatusCode, Value) {
    let auth: String;
    let mut headers = vec![("content-type", "application/json")];
    if let Some(t) = token {
        auth = format!("Bearer {}", t);
        headers.push(("authorization", auth.as_str()));
    }
    let h: Vec<(&str, &str)> = headers;
    let resp = axum_helpers::send_request(
        app,
        Method::POST,
        uri,
        h,
        Body::from(serde_json::to_string(body_val).unwrap()),
    )
    .await;
    (resp.status(), body_to_json(resp).await)
}

// ── Cross-tenant get_user tests ──────────────────────────────────

#[tokio::test]
async fn test_admin_cannot_get_user_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cg_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Other Tenant"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("cg_user_b");
    let (status, body) = post_json(&app, "/api/users", Some(&sys_token), &json!({
        "email": user_email, "password": "Password123!", "name": "User B", "tenant_id": tenant_b_id,
    })).await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b = body["id"].as_str().unwrap().to_string();

    let admin_email = common::unique_email("cg_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let (status, _body) = get_json(
        &app,
        &format!("/api/users/{}", user_id_b),
        Some(&admin_token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "admin should NOT get cross-tenant user"
    );
}

#[tokio::test]
async fn test_system_can_get_user_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cgs_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Get Tenant"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("cgs_user");
    let (status, body) = post_json(&app, "/api/users", Some(&sys_token), &json!({
        "email": user_email, "password": "Password123!", "name": "Get User", "tenant_id": tenant_b_id,
    })).await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b = body["id"].as_str().unwrap().to_string();

    let (status, _body) =
        get_json(&app, &format!("/api/users/{}", user_id_b), Some(&sys_token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "system should get cross-tenant user"
    );
}

// ── Cross-tenant update_user tests ───────────────────────────────

#[tokio::test]
async fn test_admin_cannot_update_user_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cu_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Upd Tenant"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("cu_user_b");
    let (status, body) = post_json(&app, "/api/users", Some(&sys_token), &json!({
        "email": user_email, "password": "Password123!", "name": "Upd User", "tenant_id": tenant_b_id,
    })).await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b = body["id"].as_str().unwrap().to_string();

    let admin_email = common::unique_email("cu_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let (status, _body) = put_json(
        &app,
        &format!("/api/users/{}", user_id_b),
        Some(&admin_token),
        Some(&json!({
            "name": "Hacked"
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "admin should NOT update cross-tenant user"
    );
}

#[tokio::test]
async fn test_system_can_update_user_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cus_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Sys Upd"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("cus_user");
    let (status, body) = post_json(&app, "/api/users", Some(&sys_token), &json!({
        "email": user_email, "password": "Password123!", "name": "Sys Upd User", "tenant_id": tenant_b_id,
    })).await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b = body["id"].as_str().unwrap().to_string();

    let (status, _body) = put_json(
        &app,
        &format!("/api/users/{}", user_id_b),
        Some(&sys_token),
        Some(&json!({
            "name": "Updated By System"
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "system should update cross-tenant user"
    );
}

// ── Cross-tenant delete_user tests ───────────────────────────────

#[tokio::test]
async fn test_admin_cannot_delete_user_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cd_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Del Tenant"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("cd_user_b");
    let (status, body) = post_json(&app, "/api/users", Some(&sys_token), &json!({
        "email": user_email, "password": "Password123!", "name": "Del User", "tenant_id": tenant_b_id,
    })).await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b = body["id"].as_str().unwrap().to_string();

    let admin_email = common::unique_email("cd_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let (status, _body) = delete_json(
        &app,
        &format!("/api/users/{}", user_id_b),
        Some(&admin_token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "admin should NOT delete cross-tenant user"
    );
}

#[tokio::test]
async fn test_system_can_delete_user_cross_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("cds_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Sys Del"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b_id = body["id"].as_str().unwrap().to_string();

    let user_email = common::unique_email("cds_user");
    let (status, body) = post_json(&app, "/api/users", Some(&sys_token), &json!({
        "email": user_email, "password": "Password123!", "name": "Sys Del User", "tenant_id": tenant_b_id,
    })).await;
    assert_eq!(status, StatusCode::OK);
    let user_id_b = body["id"].as_str().unwrap().to_string();

    let (status, _body) =
        delete_json(&app, &format!("/api/users/{}", user_id_b), Some(&sys_token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "system should delete cross-tenant user"
    );
}

// ── Cross-tenant list_users isolation ────────────────────────────

#[tokio::test]
async fn test_admin_list_users_tenant_scoped() {
    let app = axum_helpers::create_app().await;
    let admin_email = common::unique_email("cl_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    // Create a user via admin (will be in default tenant)
    let user_email = common::unique_email("cl_user");
    let (status, _body) = post_json(
        &app,
        "/api/users",
        Some(&admin_token),
        &json!({
            "email": user_email, "password": "Password123!", "name": "Tenant A User",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get_json(&app, "/api/users?page=1&per_page=50", Some(&admin_token)).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items should be an array");
    assert!(!items.is_empty(), "admin should see users in their tenant");
    for user in items {
        assert_eq!(
            user["tenant_id"].as_str(),
            Some("default"),
            "admin should only see users in default tenant"
        );
    }
}

// ── Move user tenant tests ───────────────────────────────────────

/// Helper: decode JWT claims to read token_version.
fn decode_token_version(token: &str, secret: &str) -> (i64, String, i32) {
    use ains_runtime::auth::JwtClaims;
    use jsonwebtoken::{DecodingKey, decode};
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    validation.set_issuer(&["ains-server"]);
    validation.set_audience(&["ains"]);
    let data = decode::<JwtClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .expect("Failed to decode JWT");
    let uid: i64 = data.claims.sub.parse().expect("Invalid user ID");
    (uid, data.claims.tenant_id, data.claims.token_version)
}

#[tokio::test]
async fn test_system_move_user_between_tenants() {
    let app = axum_helpers::create_app().await;
    let config = common::load_test_config();
    let sys_email = common::unique_email("mv_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create two tenants
    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Source Tenant"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_a = body["id"].as_str().unwrap().to_string();

    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Target Tenant"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b = body["id"].as_str().unwrap().to_string();

    // Create a user in tenant_a
    let user_email = common::unique_email("mv_user");
    let (status, body) = post_json(
        &app,
        "/api/users",
        Some(&sys_token),
        &json!({
            "email": user_email, "password": "Password123!", "name": "Moveable User",
            "tenant_id": tenant_a,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["tenant_id"], tenant_a);

    // Get token_version before move
    let user_token = axum_helpers::login(&app, &user_email).await;
    let (_uid, _, tv_before) = decode_token_version(&user_token, &config.jwt_secret);

    // System moves user to tenant_b
    let (status, body) = put_json(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&sys_token),
        Some(&json!({"tenant_id": tenant_b})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "system should move user between tenants"
    );
    assert_eq!(
        body["tenant_id"].as_str(),
        Some(tenant_b.as_str()),
        "user should now be in tenant_b"
    );

    // Re-login to get new token with incremented token_version
    let new_token = axum_helpers::login(&app, &user_email).await;
    let (_, _, tv_after) = decode_token_version(&new_token, &config.jwt_secret);
    assert!(
        tv_after > tv_before,
        "token_version should be incremented after tenant move ({} > {})",
        tv_after,
        tv_before
    );

    // Old JWT should be rejected
    let (status, _body) = get_json(&app, "/api/users/me", Some(&user_token)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "old JWT must be rejected after tenant move"
    );

    // New JWT should work
    let (status, _body) = get_json(&app, "/api/users/me", Some(&new_token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "new JWT should work after tenant move"
    );
}

#[tokio::test]
async fn test_move_user_same_tenant_noop() {
    // Moving a user to the same tenant they are already in should NOT
    // increment token_version (noop guard).
    let app = axum_helpers::create_app().await;
    let config = common::load_test_config();
    let sys_email = common::unique_email("mv_noop_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a user in the default tenant
    let user_email = common::unique_email("mv_noop_user");
    let (status, body) = post_json(
        &app,
        "/api/users",
        Some(&sys_token),
        &json!({
            "email": user_email, "password": "Password123!", "name": "Noop User",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id = body["id"].as_str().unwrap().to_string();

    // Login and read token_version
    let token = axum_helpers::login(&app, &user_email).await;
    let (_uid, _tid, tv_before) = decode_token_version(&token, &config.jwt_secret);

    // Move to the same tenant (no-op)
    let (status, body) = put_json(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&sys_token),
        Some(&json!({"tenant_id": "default"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant_id"], "default");

    // Re-login and verify token_version is unchanged
    let new_token = axum_helpers::login(&app, &user_email).await;
    let (_, _, tv_after) = decode_token_version(&new_token, &config.jwt_secret);
    assert_eq!(
        tv_after, tv_before,
        "token_version should NOT change on same-tenant move (no-op guard)"
    );
}

#[tokio::test]
async fn test_admin_cannot_move_user_to_other_tenant() {
    let app = axum_helpers::create_app().await;
    let sys_email = common::unique_email("mv_adm_sys");
    let sys_token = axum_helpers::create_system_and_login(&app, &sys_email).await;

    // Create a second tenant
    let (status, body) = post_json(
        &app,
        "/api/tenants",
        Some(&sys_token),
        &json!({"name": "Restricted Tenant"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tenant_b = body["id"].as_str().unwrap().to_string();

    // Create a regular user in default tenant (via system)
    let user_email = common::unique_email("mv_adm_user");
    let (status, body) = post_json(
        &app,
        "/api/users",
        Some(&sys_token),
        &json!({
            "email": user_email, "password": "Password123!", "name": "Restricted User",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id = body["id"].as_str().unwrap().to_string();

    // Admin from default tenant tries to move user to tenant_b — should be forbidden
    let admin_email = common::unique_email("mv_adm_admin");
    let admin_token = axum_helpers::create_admin_and_login(&app, &admin_email).await;

    let (status, _body) = put_json(
        &app,
        &format!("/api/users/{}/tenant", user_id),
        Some(&admin_token),
        Some(&json!({"tenant_id": tenant_b})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "admin should not be able to move user to another tenant"
    );
}
