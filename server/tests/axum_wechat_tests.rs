#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for the WeChat captcha-login feature.
//!
//! Covers:
//! - `GET /api/public/auth/wechat-enabled` — feature toggle endpoint
//! - `GET /api/public/wechat/callback` — WeChat server verification handshake
//! - `POST /api/public/wechat/callback` — WeChat message callback processing
//!
//! Some tests require running Redis and PostgreSQL instances.
//! Tests that require Redis are marked `#[ignore]` and can be run with:
//!   cargo test --test axum_wechat_tests -- --ignored

use std::sync::Arc;

use ains_axum::{Body, Method, Request, Router, StatusCode};
use ains_axum::{BodyExt, from_fn, from_fn_with_state};
use ains_server::middlewares::auth_middleware;
use tower::ServiceExt;

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Load the test configuration.
fn load_test_config() -> ains_server::utils::AppConfig {
    ains_server::utils::load_config("config.toml", "development")
        .expect("Failed to load config.toml for tests")
}

/// Create a test database with migrations.
async fn create_test_db() -> Arc<ains_server::AutoRouter> {
    let config = load_test_config();
    let db = sea_orm::Database::connect(&config.database_url)
        .await
        .expect("Failed to connect to database");
    let db = ains_server::AutoRouter::single(db);
    ains_server::migrations::run_migrations(db.write_conn())
        .await
        .expect("Failed to run migrations");
    ains_server::snowflake::init(db.write_conn())
        .await
        .expect("Failed to initialize Snowflake generator");
    db
}

/// Create a cache service from config.
async fn create_cache() -> ains_server::services::CacheService {
    let config = load_test_config();
    ains_server::services::CacheService::new(&config.redis_url, config.cache_max_connections).await
}

/// Build an `AppState` with WeChat components enabled and configurable captcha
/// length. Used by tests that need to verify captcha code length behaviour.
async fn create_wechat_state_with_len(
    account_id: &str,
    captcha_len: usize,
) -> ains_server::AppState {
    let config = load_test_config();
    let db = create_test_db().await;
    let cache = create_cache().await;

    // Mutate config to enable WeChat with test credentials.
    let mut wechat_cfg = config.wechat.clone();
    wechat_cfg.enabled = true;
    wechat_cfg.account_id = account_id.to_string();
    wechat_cfg.app_id = "test_app_id".to_string();
    wechat_cfg.app_secret = "test_app_secret".to_string();
    wechat_cfg.token = "test_token".to_string();
    wechat_cfg.captcha_len = captcha_len;

    let mut app_config = config;
    app_config.wechat = wechat_cfg;

    let wechat = ains_server::services::wechat::init_wechat_components(&app_config.wechat, &cache);

    let gateway = Arc::new(ains_server::services::gateway::GatewayService::new(
        db.clone(),
        &app_config.jwt_secret,
    ));

    ains_server::AppState {
        db,
        cache,
        config: Arc::new(app_config),
        email: emailserver::EmailService::new(emailserver::EmailConfig::default()),
        wechat,
        gateway,
    }
}

/// Build an `AppState` with WeChat components enabled using test credentials.
///
/// The WeChat captcha-login feature requires valid-looking credentials even
/// though no real WeChat server is involved. The `account_id` distinguishes
/// test accounts to avoid key collisions when tests run in parallel.
async fn create_wechat_state(account_id: &str) -> ains_server::AppState {
    create_wechat_state_with_len(account_id, 5).await
}

/// Build a test router with the same middleware stack as production,
/// but using the provided (WeChat-enabled) state.
fn build_router(state: ains_server::AppState) -> Router {
    use ains_axum::{CorsLayer, TraceLayer};
    use distributed_ratelimit::{RateLimitConfig, RedisRateLimiter};

    let cors = CorsLayer::new()
        .allow_origin(ains_axum::Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ])
        .allow_headers(ains_axum::Any);

    // Use the production build_app_router, but we replicate the key parts here
    // so the test is self-contained. The critical thing is that the WeChat
    // callback routes are conditionally registered based on state.wechat.

    Router::new()
        .nest(
            "/api",
            ains_server::routes::api_routes().layer(from_fn_with_state(
                state.clone(),
                auth_middleware::<ains_server::AppState>,
            )),
        )
        .nest(
            "/api/public/auth",
            ains_server::routes::auth_routes(
                RedisRateLimiter::disabled(RateLimitConfig::default()),
            ),
        )
        .merge(if state.wechat.is_some() {
            Router::new().route(
                "/api/public/wechat/callback",
                ains_axum::get(ains_server::handlers::wechat::wechat_callback_get).merge(
                    ains_axum::post(ains_server::handlers::wechat::wechat_callback_post),
                ),
            )
        } else {
            Router::new()
        })
        .layer(from_fn(ains_server::middlewares::panic_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

/// Build a simple test router WITHOUT WeChat components (wechat: None).
async fn build_router_without_wechat() -> Router {
    use ains_axum::{CorsLayer, TraceLayer};
    use distributed_ratelimit::{RateLimitConfig, RedisRateLimiter};

    let config = load_test_config();
    let db = create_test_db().await;
    let cache = create_cache().await;

    let gateway = Arc::new(ains_server::services::gateway::GatewayService::new(
        db.clone(),
        &config.jwt_secret,
    ));

    let state = ains_server::AppState {
        db,
        cache,
        config: Arc::new(config),
        email: emailserver::EmailService::new(emailserver::EmailConfig::default()),
        wechat: None,
        gateway,
    };

    let cors = CorsLayer::new()
        .allow_origin(ains_axum::Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ])
        .allow_headers(ains_axum::Any);

    Router::new()
        .nest(
            "/api",
            ains_server::routes::api_routes().layer(from_fn_with_state(
                state.clone(),
                auth_middleware::<ains_server::AppState>,
            )),
        )
        .nest(
            "/api/public/auth",
            ains_server::routes::auth_routes(
                RedisRateLimiter::disabled(RateLimitConfig::default()),
            ),
        )
        .layer(from_fn(ains_server::middlewares::panic_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

/// Parse the response body as JSON.
async fn body_to_json(response: ains_axum::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Parse the response body as UTF-8 string (for XML / text).
async fn body_text(response: ains_axum::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

/// Compute the WeChat signature for test callback parameters.
fn compute_signature(token: &str, timestamp: &str, nonce: &str) -> String {
    wechat_api::crypto::compute_signature(token, timestamp, nonce)
}

/// Extract the captcha code from a WeChat callback XML reply.
///
/// The reply XML has the format:
/// `<Content><![CDATA[你的验证码：ABCDE，有效期5分钟。...]]></Content>`
fn extract_code_from_reply(xml: &str) -> Option<&str> {
    let prefix = "你的验证码：";
    let start = xml.find(prefix)? + prefix.len();
    let after = &xml[start..];
    let end = after
        .find(|ch: char| ch == '，' || ch == ',' || ch.is_whitespace() || ch == '<')
        .unwrap_or(after.len());
    Some(&after[..end])
}

// ── Tests for unconfigured WeChat ────────────────────────────────────────────
//
// These tests verify the behaviour when `state.wechat` is `None` (the default).

#[tokio::test]
async fn test_wechat_enabled_false_when_not_configured() {
    let app = build_router_without_wechat().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/public/auth/wechat-enabled")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_to_json(response).await;
    assert_eq!(body["enabled"], false);
}

// ── Tests for configured WeChat (no Redis required) ─────────────────────────
//
// These tests use `build_router` with WeChat components enabled.
// The RedisCaptchaStore uses a graceful no-op mode when Redis is unavailable,
// so all store operations return None / no-op without error.

#[tokio::test]
async fn test_wechat_enabled_true_when_configured() {
    let state = create_wechat_state("test_enabled_true").await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/public/auth/wechat-enabled")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_to_json(response).await;
    assert_eq!(body["enabled"], true);
}

#[tokio::test]
async fn test_callback_get_valid_signature() {
    let state = create_wechat_state("test_cb_get_valid").await;
    let app = build_router(state);
    let token = "test_token";
    let ts = "1609459200";
    let nonce = "nonce123";
    let sig = compute_signature(token, ts, nonce);

    let uri = format!(
        "/api/public/wechat/callback?signature={}&timestamp={}&nonce={}&echostr=HELLO_ECHO",
        sig, ts, nonce
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "valid signature should return 200"
    );
    let text = body_text(response).await;
    assert_eq!(
        text, "HELLO_ECHO",
        "response body must be the echostr value"
    );
}

#[tokio::test]
async fn test_callback_get_invalid_signature() {
    let state = create_wechat_state("test_cb_get_invalid").await;
    let app = build_router(state);

    let uri = "/api/public/wechat/callback?signature=BAD&timestamp=1&nonce=n&echostr=ECHO";

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "invalid signature should return 400"
    );
}

#[tokio::test]
async fn test_callback_post_trigger_keyword() {
    let state = create_wechat_state("test_cb_post_trig").await;
    let app = build_router(state);
    let token = "test_token";
    let ts = "1609459200";
    let nonce = "nonce456";
    let sig = compute_signature(token, ts, nonce);

    let uri = format!(
        "/api/public/wechat/callback?signature={}&timestamp={}&nonce={}",
        sig, ts, nonce
    );

    // XML body with a trigger keyword (验证码).
    let body = r#"<xml><ToUserName><![CDATA[gh_test]]></ToUserName><FromUserName><![CDATA[oTriggerUser]]></FromUserName><CreateTime>1609459200</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[验证码]]></Content><MsgId>1234567890</MsgId></xml>"#;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("content-type", "application/xml")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "callback with trigger keyword should return 200"
    );

    // Response must be XML containing a verification code.
    let text = body_text(response).await;
    assert!(
        text.contains("<Content><![CDATA["),
        "response must contain a text reply XML, got: {}",
        text
    );
    assert!(
        text.contains("verification code") || text.contains("验证码"),
        "response must mention captcha code, got: {}",
        text
    );
}

#[tokio::test]
async fn test_callback_post_non_keyword() {
    let state = create_wechat_state("test_cb_post_help").await;
    let app = build_router(state);
    let token = "test_token";
    let ts = "1609459200";
    let nonce = "nonce789";
    let sig = compute_signature(token, ts, nonce);

    let uri = format!(
        "/api/public/wechat/callback?signature={}&timestamp={}&nonce={}",
        sig, ts, nonce
    );

    // XML body with a non-trigger text message.
    let body = r#"<xml><ToUserName><![CDATA[gh_test]]></ToUserName><FromUserName><![CDATA[oHelpUser]]></FromUserName><CreateTime>1609459200</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[hello world]]></Content><MsgId>1234567891</MsgId></xml>"#;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("content-type", "application/xml")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "callback with non-keyword should return 200"
    );

    // Non-trigger messages must be handed off to WeChat's AI reply service
    // (transfer_biz_ai_ivr) rather than answered by our own text reply, so
    // the account's configured AI reply is not suppressed.
    let text = body_text(response).await;
    assert!(
        text.contains("transfer_biz_ai_ivr"),
        "response must transfer to AI reply, got: {}",
        text
    );
}

#[tokio::test]
async fn test_callback_post_subscribe_event() {
    let state = create_wechat_state("test_cb_post_subscribe").await;
    // The default test config keeps `subscribe_reply` non-empty, so a subscribe
    // event must be answered with our configured welcome text rather than being
    // handed over to the AI reply service.
    let expected_reply = state.wechat.as_ref().unwrap().subscribe_reply.clone();
    assert!(
        !expected_reply.is_empty(),
        "test precondition: subscribe_reply must be non-empty by default"
    );
    let app = build_router(state);
    let token = "test_token";
    let ts = "1609459200";
    let nonce = "noncesub1";
    let sig = compute_signature(token, ts, nonce);

    let uri = format!(
        "/api/public/wechat/callback?signature={}&timestamp={}&nonce={}",
        sig, ts, nonce
    );

    // XML body for a `subscribe` event (a user follows the official account).
    let body = r#"<xml><ToUserName><![CDATA[gh_test]]></ToUserName><FromUserName><![CDATA[oSubUser]]></FromUserName><CreateTime>1609459200</CreateTime><MsgType><![CDATA[event]]></MsgType><Event><![CDATA[subscribe]]></Event></xml>"#;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("content-type", "application/xml")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "subscribe event should return 200"
    );

    // A subscribe event must be answered with the configured welcome text and
    // must NOT be transferred to the AI reply service.
    let text = body_text(response).await;
    assert!(
        text.contains(&expected_reply),
        "subscribe response must contain the configured welcome text, got: {}",
        text
    );
    assert!(
        !text.contains("transfer_biz_ai_ivr"),
        "subscribe response must not transfer to AI reply, got: {}",
        text
    );
}

#[tokio::test]
async fn test_callback_post_menu_click_captcha() {
    // Tapping the custom-menu CLICK button whose key is `GET_AINS_CAPTCHA`
    // must route into the captcha branch (a text reply), exactly like the
    // trigger keyword — NOT the AI-transfer or the bare `success` ack.
    let state = create_wechat_state("test_cb_post_click").await;
    let app = build_router(state);
    let token = "test_token";
    let ts = "1609459200";
    let nonce = "nonceclick";
    let sig = compute_signature(token, ts, nonce);

    let uri = format!(
        "/api/public/wechat/callback?signature={}&timestamp={}&nonce={}",
        sig, ts, nonce
    );

    // XML body for a menu CLICK event carrying the captcha button key.
    let body = r#"<xml><ToUserName><![CDATA[gh_test]]></ToUserName><FromUserName><![CDATA[oClickUser]]></FromUserName><CreateTime>1609459200</CreateTime><MsgType><![CDATA[event]]></MsgType><Event><![CDATA[CLICK]]></Event><EventKey><![CDATA[GET_AINS_CAPTCHA]]></EventKey></xml>"#;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("content-type", "application/xml")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "menu CLICK captcha event should return 200"
    );

    // The captcha branch always answers with a text reply (either the code or,
    // if Redis is unavailable, a busy message) — both use build_text_reply.
    // It must never be an AI transfer or a bare ack.
    let text = body_text(response).await;
    assert!(
        text.contains("<Content><![CDATA["),
        "menu CLICK captcha event must produce a text reply, got: {}",
        text
    );
    assert!(
        !text.contains("transfer_biz_ai_ivr"),
        "menu CLICK captcha event must not transfer to AI reply, got: {}",
        text
    );
    assert_ne!(
        text, "success",
        "menu CLICK captcha event must not be a bare ack"
    );
}

#[tokio::test]
async fn test_callback_post_other_event_acked() {
    // Non-chat events that are neither a captcha trigger nor a subscribe (e.g.
    // a menu CLICK with an unrelated key, VIEW/LOCATION/SCAN reports) must be
    // acknowledged with a bare `success` — NOT transferred to the AI reply
    // service, which would push an unwanted message or produce an error reply.
    let state = create_wechat_state("test_cb_post_otherevt").await;
    let app = build_router(state);
    let token = "test_token";
    let ts = "1609459200";
    let nonce = "nonceotherevt";
    let sig = compute_signature(token, ts, nonce);

    let uri = format!(
        "/api/public/wechat/callback?signature={}&timestamp={}&nonce={}",
        sig, ts, nonce
    );

    // XML body for a menu CLICK event whose key is NOT the captcha button.
    let body = r#"<xml><ToUserName><![CDATA[gh_test]]></ToUserName><FromUserName><![CDATA[oOtherEvtUser]]></FromUserName><CreateTime>1609459200</CreateTime><MsgType><![CDATA[event]]></MsgType><Event><![CDATA[CLICK]]></Event><EventKey><![CDATA[SOME_OTHER_BUTTON]]></EventKey></xml>"#;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("content-type", "application/xml")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "non-chat event should return 200"
    );

    let text = body_text(response).await;
    assert_eq!(
        text, "success",
        "non-chat event must be acked with a bare 'success', got: {}",
        text
    );
    assert!(
        !text.contains("transfer_biz_ai_ivr"),
        "non-chat event must not transfer to AI reply, got: {}",
        text
    );
}

#[tokio::test]
async fn test_callback_post_malformed_xml() {
    let state = create_wechat_state("test_cb_post_badxml").await;
    let app = build_router(state);
    let token = "test_token";
    let ts = "1609459200";
    let nonce = "nonce000";
    let sig = compute_signature(token, ts, nonce);

    let uri = format!(
        "/api/public/wechat/callback?signature={}&timestamp={}&nonce={}",
        sig, ts, nonce
    );

    // Malformed XML body.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("content-type", "application/xml")
                .body(Body::from("not xml at all".to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "malformed XML should still return 200 (handler absorbs errors to suppress WeChat retries)"
    );

    // The handler always returns 200 with either the reply XML or "success".
    let text = body_text(response).await;
    assert_eq!(
        text, "success",
        "malformed XML should produce 'success' fallback body"
    );
}

// ── Tests requiring Redis (ignored by default) ─────────────────────────────
//
// Run with: cargo test --test axum_wechat_tests -- --ignored

#[tokio::test]
#[ignore]
async fn test_captcha_len_configuration() {
    // Valid captcha code characters, matching wechat-api::captcha::CHARSET.
    let valid_chars: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

    let account_id = "test_captcha_len";
    let state = create_wechat_state_with_len(account_id, 6).await;
    let app = build_router(state);
    let token = "test_token";
    let ts = "1609459200";
    let nonce = "nonce_clen";
    let sig = compute_signature(token, ts, nonce);

    let uri = format!(
        "/api/public/wechat/callback?signature={}&timestamp={}&nonce={}",
        sig, ts, nonce
    );

    // XML body with a trigger keyword (验证码).
    let body = r#"<xml><ToUserName><![CDATA[gh_test]]></ToUserName><FromUserName><![CDATA[oCaptchaLenUser]]></FromUserName><CreateTime>1609459200</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[验证码]]></Content><MsgId>1234567892</MsgId></xml>"#;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("content-type", "application/xml")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "callback with trigger keyword should return 200"
    );

    let text = body_text(response).await;
    let code = extract_code_from_reply(&text).expect("response must contain a verification code");

    assert_eq!(
        code.len(),
        6,
        "captcha code length must match configured captcha_len=6, got: '{}'",
        code
    );
    assert!(
        code.bytes().all(|b| valid_chars.contains(&b)),
        "code '{}' contains characters outside the valid charset",
        code
    );
}
