//! Salvo-specific test harness — creates test app and sends requests via reqwest.

#![allow(dead_code)]

use ains_salvo::SalvoRuntime;
use ains_server::{AppState, Runtime};
use distributed_ratelimit::RedisRateLimiter;
use serde_json::Value;
use std::net::TcpListener;
use std::sync::Arc;

use crate::common;
use crate::common::DEFAULT_TEST_PASSWORD;

/// A test server that runs on a random port and sends responses via reqwest.
pub struct TestServer {
    /// Base URL for this server (e.g. "http://127.0.0.1:54321").
    pub base_url: String,
    /// The reqwest client (shared, connection-pooled).
    pub client: reqwest::Client,
    /// Holds the server task handle — dropped when TestServer goes out of scope.
    _server_handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

/// Start a test server on a random port and return a TestServer handle.
///
/// Uses the production `bootstrap::salvo::build_app_router()` to build the
/// middleware chain, then serves via `SalvoRuntime::serve()`. This ensures
/// the test server stays in sync with the production middleware chain.
pub async fn create_test_server() -> TestServer {
    // 获取随机可用端口后立即释放（std TcpListener 仅用于探测端口），
    // 否则 SalvoTcpListener 后续绑定同一端口会报 EADDRINUSE。
    let (addr, base_url) = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind to random port");
        let addr = listener.local_addr().expect("Failed to get local address");
        let base_url = format!("http://{}", addr);
        (addr, base_url)
    };

    let db = common::create_test_db_and_run_migrations().await;
    let cache = common::create_cache_service().await;
    let config = Arc::new(common::load_test_config());

    let gateway = Arc::new(
        ains_server::services::gateway::GatewayService::new_with_proxy_flag(
            db.clone(),
            &config.jwt_secret,
            true, /* no_proxy — loopback provider tests must bypass system proxy */
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

    // 使用生产级的 build_app_router，注入禁用的 rate limiter 以避免测试中
    // 因同一 IP（127.0.0.1）发送过多请求而触发限流。
    let rate_limiter =
        RedisRateLimiter::disabled(distributed_ratelimit::RateLimitConfig::default());
    let router =
        ains_server::bootstrap::salvo::build_app_router(state.clone(), "development", rate_limiter);

    // 使用 SalvoRuntime::serve() 启动（与生产代码一致的状态注入方式）
    let _server_handle = tokio::spawn(async move {
        SalvoRuntime::<AppState>::serve(router, state, &addr.to_string())
            .await
            .expect("Salvo test server failed");
    });

    // Build the reqwest client with no_proxy for test isolation
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .expect("Failed to build reqwest client");

    // Quick health check — wait for the server to be ready
    let health_url = format!("{}/api/health", base_url);
    for attempt in 0..20 {
        if let Ok(resp) = client.get(&health_url).send().await
            && resp.status().is_success()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if attempt == 19 {
            panic!("Test server failed to start at {}", base_url);
        }
    }

    TestServer {
        base_url,
        client,
        _server_handle,
    }
}

/// Send a JSON POST request and return the response as (status, body).
pub async fn post_json(
    server: &TestServer,
    path: &str,
    body: &Value,
) -> (reqwest::StatusCode, Value) {
    let url = format!("{}{}", server.base_url, path);
    let resp = server
        .client
        .post(&url)
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .expect("Failed to send POST request");

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Send a GET request and return the response as (status, body).
pub async fn get(
    server: &TestServer,
    path: &str,
    token: Option<&str>,
) -> (reqwest::StatusCode, Value) {
    let url = format!("{}{}", server.base_url, path);
    let mut req = server.client.get(&url);
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {}", token));
    }
    let resp = req.send().await.expect("Failed to send GET request");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Send a POST request with optional bearer token and JSON body.
pub async fn post(
    server: &TestServer,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (reqwest::StatusCode, Value) {
    let url = format!("{}{}", server.base_url, path);
    let mut req = server
        .client
        .post(&url)
        .header("content-type", "application/json");
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {}", token));
    }
    if let Some(body) = body {
        req = req.json(body);
    }
    let resp = req.send().await.expect("Failed to send POST request");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Send a PUT request with optional bearer token and JSON body.
pub async fn put(
    server: &TestServer,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (reqwest::StatusCode, Value) {
    let url = format!("{}{}", server.base_url, path);
    let mut req = server
        .client
        .put(&url)
        .header("content-type", "application/json");
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {}", token));
    }
    if let Some(body) = body {
        req = req.json(body);
    }
    let resp = req.send().await.expect("Failed to send PUT request");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Send a DELETE request with optional bearer token.
pub async fn delete(
    server: &TestServer,
    path: &str,
    token: Option<&str>,
) -> (reqwest::StatusCode, Value) {
    let url = format!("{}{}", server.base_url, path);
    let mut req = server.client.delete(&url);
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {}", token));
    }
    let resp = req.send().await.expect("Failed to send DELETE request");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Register a user and return JWT token.
pub async fn register_and_login(server: &TestServer, email: &str) -> String {
    let register_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD,
        "password_confirm": DEFAULT_TEST_PASSWORD,
        "name": "Test User"
    });

    let (status, _) = post_json(server, "/api/public/auth/register", &register_payload).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let login_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD
    });

    let (status, body) = post_json(server, "/api/public/auth/login", &login_payload).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    body["token"].as_str().unwrap().to_string()
}

/// Create a system-role user in the database and return the JWT token.
/// Mirrors `common::axum::create_system_and_login`.
pub async fn create_system_and_login(server: &TestServer, email: &str) -> String {
    use ains_runtime::auth::JwtClaims;
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    let token = register_and_login(server, email).await;
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
    let (status, body) = post_json(server, "/api/public/auth/login", &login_payload).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    body["token"].as_str().unwrap().to_string()
}

/// Create a test AppState for service-level tests (no HTTP server needed).
pub async fn create_test_state() -> ains_server::AppState {
    let db = crate::common::create_test_db_and_run_migrations().await;
    let cache = crate::common::create_cache_service().await;
    let config = std::sync::Arc::new(crate::common::load_test_config());
    let gateway = std::sync::Arc::new(ains_server::services::gateway::GatewayService::new(
        db.clone(),
        &config.jwt_secret,
    ));
    ains_server::AppState {
        db,
        cache,
        config,
        email: crate::common::default_email_service(),
        wechat: None,
        gateway,
    }
}

/// Register and login with remember=true, returning (jwt, refresh_token).
pub async fn register_and_login_with_refresh(server: &TestServer, email: &str) -> (String, String) {
    let register_payload = serde_json::json!({
        "email": email,
        "password": DEFAULT_TEST_PASSWORD,
        "password_confirm": DEFAULT_TEST_PASSWORD,
        "name": "Test User"
    });
    let (status, _) = post_json(server, "/api/public/auth/register", &register_payload).await;
    assert_eq!(status, reqwest::StatusCode::OK);

    let url = format!("{}{}", server.base_url, "/api/public/auth/login");
    let login_resp = server
        .client
        .post(&url)
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "email": email,
            "password": DEFAULT_TEST_PASSWORD,
            "remember": true,
        }))
        .send()
        .await
        .expect("Failed to send login request");
    assert_eq!(login_resp.status(), reqwest::StatusCode::OK);

    // Extract refresh token from Set-Cookie headers before consuming the response
    let refresh_token = login_resp
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

    let login_body: serde_json::Value = login_resp.json().await.unwrap();
    let jwt = login_body["token"].as_str().unwrap().to_string();

    (jwt, refresh_token)
}
