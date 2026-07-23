//! Axum-specific test harness — creates test app and sends requests via tower::ServiceExt.

#![allow(dead_code)]

use ains_axum::{Body, BodyExt, Method, Request, Router, StatusCode};
use ains_server::AppState;
use ains_server::services::{MeteringService, QuotaService};
use ains_server::utils::config::QuotaConfig;
use distributed_ratelimit::{RateLimitConfig, RedisRateLimiter};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

use crate::common;
use crate::common::DEFAULT_TEST_PASSWORD;

/// Create a test router with the same middleware stack as the production app.
pub async fn create_app() -> Router {
    create_app_and_state().await.0
}

/// Create test router and return both Router and AppState for direct state inspection.
///
/// Uses the production `bootstrap::axum::build_app_router()` to build the
/// middleware chain. This ensures the test server stays in sync with the
/// production middleware chain. A disabled rate limiter is injected so that
/// tests do not hit per-IP rate limits (all requests originate from localhost).
pub async fn create_app_and_state() -> (Router, AppState) {
    let db = common::create_test_db_and_run_migrations().await;
    let cache = common::create_cache_service().await;
    let config = Arc::new(common::load_test_config());

    let gateway = Arc::new(
        ains_server::services::gateway::GatewayService::new_with_proxy_flag(
            db.clone(),
            &config.jwt_secret,
            true, /* no_proxy — tests always bypass system proxy */
        ),
    );

    let state = AppState {
        db,
        cache,
        config,
        email: common::default_email_service(),
        wechat: None,
        gateway,
    };

    // 使用生产级的 build_app_router，注入禁用的 rate limiter
    let rate_limiter =
        RedisRateLimiter::disabled(distributed_ratelimit::RateLimitConfig::default());
    let router =
        ains_server::bootstrap::axum::build_app_router(state.clone(), "development", rate_limiter);

    // Axum 的 build_app_router 不调用 .with_state()（与 Salvo 端对称），
    // 测试中通过 oneshot 发送请求前需要注入状态。
    let router = router.with_state(state.clone());

    (router, state)
}

/// Send an HTTP request through the axum router and return the response.
pub async fn send_request(
    app: &Router,
    method: Method,
    uri: &str,
    headers: Vec<(&str, &str)>,
    body: Body,
) -> ains_axum::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    for (key, value) in headers {
        builder = builder.header(key, value);
    }
    let request = builder.body(body).unwrap();
    app.clone().oneshot(request).await.unwrap()
}

/// Send a JSON body POST request through the axum router.
pub async fn send_json_post(app: &Router, uri: &str, body: &Value) -> ains_axum::Response {
    send_request(
        app,
        Method::POST,
        uri,
        vec![("content-type", "application/json")],
        Body::from(serde_json::to_string(body).unwrap()),
    )
    .await
}

/// Extract JSON body from axum Response.
pub async fn body_to_json(response: ains_axum::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Extract body bytes from axum Response.
pub async fn body_bytes(response: ains_axum::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

/// Login with an existing user and return JWT token (no registration).
pub async fn login(app: &Router, email: &str) -> String {
    let login_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD
    });
    let login_response = send_json_post(app, "/api/public/auth/login", &login_payload).await;
    assert_eq!(
        login_response.status(),
        StatusCode::OK,
        "login should succeed for {}",
        email
    );
    let login_body = body_to_json(login_response).await;
    login_body["token"].as_str().unwrap().to_string()
}

/// Register a user and return JWT token.
pub async fn register_and_login(app: &Router, email: &str) -> String {
    let register_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD,
        "password_confirm": DEFAULT_TEST_PASSWORD,
        "name": "Test User"
    });

    let register_response =
        send_json_post(app, "/api/public/auth/register", &register_payload).await;
    assert_eq!(register_response.status(), StatusCode::OK);

    let login_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD
    });

    let login_response = send_json_post(app, "/api/public/auth/login", &login_payload).await;
    assert_eq!(login_response.status(), StatusCode::OK);

    let login_body = body_to_json(login_response).await;
    login_body["token"].as_str().unwrap().to_string()
}

/// Register a user, then log in with `remember=true` and extract both the
/// JWT and the refresh-token cookie from the response. Returns `(jwt, refresh_token)`.
pub async fn register_and_login_with_refresh(app: &Router, email: &str) -> (String, String) {
    let register_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD,
        "password_confirm": DEFAULT_TEST_PASSWORD,
        "name": "Test User"
    });

    let resp = send_json_post(app, "/api/public/auth/register", &register_payload).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Login with remember=true to get JWT + refresh token
    let login_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD,
        "remember": true,
    });

    let resp = send_json_post(app, "/api/public/auth/login", &login_payload).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Extract refresh token from Set-Cookie headers.
    let refresh_token = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|h| h.to_str().ok())
        .filter_map(|cookie_str| {
            if let Some(value_start) = cookie_str.find("ains_refresh") {
                let after_name = &cookie_str[value_start + "ains_refresh".len()..];
                if after_name.starts_with('=') {
                    let value_end = after_name.find(';').unwrap_or(after_name.len());
                    Some(after_name[1..value_end].to_string())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .next()
        .expect("Set-Cookie for ains_refresh not found");

    // Parse the JSON body for the JWT.
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let jwt = body["token"].as_str().unwrap().to_string();

    (jwt, refresh_token)
}

/// Create a system-role user in the database and return the JWT token.
pub async fn create_system_and_login(app: &Router, email: &str) -> String {
    use ains_runtime::auth::JwtClaims;
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    let token = register_and_login(app, email).await;
    let secret = common::load_test_config().jwt_secret;

    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    validation.set_issuer(&["ains-server"]);
    validation.set_audience(&["ains"]);
    let token_data = jsonwebtoken::decode::<JwtClaims>(
        &token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .expect("Failed to decode token");

    let user_id: i64 = token_data.claims.sub.parse().expect("Invalid user ID");

    let config = common::load_test_config();
    let db = sea_orm::Database::connect(&config.database_url)
        .await
        .expect("Failed to connect to database");

    // Atomic SQL UPDATE — avoids the read-modify-write pattern (token_version = token_version + 1)
    // that would otherwise be required by the ActiveModel approach.
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE users SET role = $1, token_version = token_version + 1, updated_at = NOW() WHERE id = $2",
        ["system".into(), user_id.into()],
    ))
    .await
    .expect("Failed to update user to system");

    // Re-login to get a new token with the updated role
    let login_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD
    });
    let login_response = send_json_post(app, "/api/public/auth/login", &login_payload).await;
    assert_eq!(login_response.status(), StatusCode::OK);
    let login_body = body_to_json(login_response).await;
    login_body["token"].as_str().unwrap().to_string()
}

/// Send an authenticated JSON POST and return `(status, parsed_body)`.
async fn auth_json_post(app: &Router, uri: &str, token: &str, body: &Value) -> (StatusCode, Value) {
    let auth = format!("Bearer {token}");
    let resp = send_request(
        app,
        Method::POST,
        uri,
        vec![
            ("content-type", "application/json"),
            ("authorization", &auth),
        ],
        Body::from(serde_json::to_string(body).unwrap()),
    )
    .await;
    let status = resp.status();
    (status, body_to_json(resp).await)
}

/// Create an isolated tenant that has no channels, create a user inside it, and
/// return that user's JWT token.
///
/// Self-registered users all land in the shared `default` tenant, where sibling
/// tests concurrently create AI channels. Any test that needs a hermetic
/// "no active channel" premise must therefore use a dedicated tenant — otherwise
/// the request may pick up an unrelated channel and fail with an upstream error
/// (a flaky 4xx/5xx instead of the expected 503 `NoChannel`).
pub async fn register_isolated_tenant_user(app: &Router, label: &str) -> String {
    let sys_email = common::unique_email(&format!("{label}_sys"));
    let sys_token = create_system_and_login(app, &sys_email).await;

    let tenant_name = common::unique_table_name(label);
    let (status, body) = auth_json_post(
        app,
        "/api/tenants",
        &sys_token,
        &serde_json::json!({ "name": tenant_name }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "isolated tenant creation should succeed"
    );
    let tenant_id = body["id"].as_str().expect("tenant id").to_string();

    let email = common::unique_email(label);
    let (status, _) = auth_json_post(
        app,
        "/api/users",
        &sys_token,
        &serde_json::json!({
            "email": email,
            "password": DEFAULT_TEST_PASSWORD,
            "name": "Isolated Tenant User",
            "tenant_id": tenant_id,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "isolated tenant user creation should succeed"
    );

    login(app, &email).await
}

/// Create an admin user in the database and return the JWT token.
pub async fn create_admin_and_login(app: &Router, email: &str) -> String {
    use ains_runtime::auth::JwtClaims;
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    // Register normally first
    let token = register_and_login(app, email).await;

    // Decode token to get user_id
    let secret = common::load_test_config().jwt_secret;
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    validation.set_issuer(&["ains-server"]);
    validation.set_audience(&["ains"]);
    let token_data = jsonwebtoken::decode::<JwtClaims>(
        &token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .expect("Failed to decode token");

    let user_id: i64 = token_data
        .claims
        .sub
        .parse()
        .expect("Invalid user ID in token");

    // Connect to DB directly to update the user role to admin using atomic SQL.
    // Uses token_version = token_version + 1 to match the production pattern.
    let config = common::load_test_config();
    let db = sea_orm::Database::connect(&config.database_url)
        .await
        .expect("Failed to connect to database");

    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE users SET role = $1, token_version = token_version + 1, updated_at = NOW() WHERE id = $2",
        ["admin".into(), user_id.into()],
    ))
    .await
    .expect("Failed to update user to admin");

    // Re-login to get a new token with the updated role
    let login_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD
    });
    let login_response = send_json_post(app, "/api/public/auth/login", &login_payload).await;
    assert_eq!(login_response.status(), StatusCode::OK);
    let login_body = body_to_json(login_response).await;
    login_body["token"].as_str().unwrap().to_string()
}

/// Create a test router with quota and metering services enabled.
///
/// Unlike `create_app_and_state()` which uses a disabled rate limiter and
/// gateway without quota, this helper enables the full quota stack (RPM/TPM
/// checks and circuit breaker) and token metering, backed by real Redis.
///
/// The `quota_config` parameter allows tests to set custom limits (e.g.,
/// a low `cb_failure_threshold` or `channel_max_rpm`) without modifying
/// the shared config.toml.
pub async fn create_app_with_quota(quota_config: QuotaConfig) -> Router {
    let db = common::create_test_db_and_run_migrations().await;
    let cache = common::create_cache_service().await;
    let config = Arc::new(common::load_test_config());

    // Create a real Redis rate limiter from the cache service's Redis client.
    let rate_limiter = cache
        .redis_client()
        .map(|client| RedisRateLimiter::new(client.clone(), RateLimitConfig::default()));

    let quota = QuotaService::new(rate_limiter, cache.clone(), Arc::new(quota_config));
    let metering = MeteringService::new(db.clone());
    let secret = if !config.gateway_encryption_key.is_empty() {
        &config.gateway_encryption_key
    } else {
        &config.jwt_secret
    };
    let gateway = Arc::new(ains_server::services::gateway::GatewayService::with_quota(
        db.clone(),
        secret,
        quota,
        Some(metering),
        true, /* no_proxy — tests always bypass system proxy */
    ));

    let state = AppState {
        db,
        cache,
        config,
        email: common::default_email_service(),
        wechat: None,
        gateway,
    };

    let limiter = RedisRateLimiter::disabled(distributed_ratelimit::RateLimitConfig::default());
    let router =
        ains_server::bootstrap::axum::build_app_router(state.clone(), "development", limiter);

    router.with_state(state.clone())
}
