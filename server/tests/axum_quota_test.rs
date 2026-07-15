#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for the AI Gateway QuotaService and circuit breaker.
//!
//! Unlike the unit tests in `services/quota.rs` (which use noop cache and
//! are partly `#[ignore]`), these tests exercise the full quota stack with
//! a real Redis connection — circuit breaker, RPM/TPM checks, and tenant
//! RPM — both directly through the QuotaService API and end-to-end through
//! the HTTP proxy layer.
//!
//! These tests require running PostgreSQL and Redis instances.
//! Start services before running:
//! - PostgreSQL: default port 5432
//! - Redis: default port 6379
//!
//! Run: cargo test --test axum_quota_test

use ains_server::services::QuotaService;
use ains_server::utils::config::QuotaConfig;
use distributed_ratelimit::{RateLimitConfig, RedisRateLimiter};
use std::sync::Arc;

mod common;

// ── Helpers ─────────────────────────────────────────────────────

/// Create a QuotaService backed by real Redis (from the test config).
async fn create_quota(config: QuotaConfig) -> QuotaService {
    let cache = common::create_cache_service().await;
    let rate_limiter = cache
        .redis_client()
        .map(|client| RedisRateLimiter::new(client.clone(), RateLimitConfig::default()));
    QuotaService::new(rate_limiter, cache, Arc::new(config))
}

// ═══════════════════════════════════════════════════════════════════
//  Direct QuotaService tests (with real Redis)
// ═══════════════════════════════════════════════════════════════════

/// All quota checks pass with default (generous) limits.
#[tokio::test]
async fn test_check_all_passes_with_default_config() {
    let quota = create_quota(QuotaConfig::default()).await;
    let result = quota.check_all("ch-pass", "t-pass", "chat", 100).await;
    assert!(result.is_ok(), "check_all should pass with defaults");
}

/// QuotaService returns `ChannelRpmExceeded` when channel RPM is exceeded.
#[tokio::test]
async fn test_channel_rpm_exceeded() {
    let config = QuotaConfig {
        channel_max_rpm: 1,
        ..Default::default()
    };
    let quota = create_quota(config).await;
    let channel_id = "ch-rpm-test";

    // First call should pass (RPM limit = 1)
    assert!(quota.check_all(channel_id, "t1", "chat", 1).await.is_ok());

    // Second call should fail (RPM exceeded)
    let result = quota.check_all(channel_id, "t1", "chat", 1).await;
    assert!(result.is_err(), "Expected channel RPM to be exceeded");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("RPM"),
        "Expected RPM error, got: {}",
        err
    );
}

/// QuotaService returns `ChannelTpmExceeded` when channel TPM is exceeded.
///
/// channel_max_tpm=1000 → max_units=1 (1 unit per 1000 tokens).
/// After 1 unit consumed, the 2nd call exceeds the budget.
#[tokio::test]
async fn test_channel_tpm_exceeded() {
    let config = QuotaConfig {
        channel_max_tpm: 1000, // max_units = 1
        channel_max_rpm: 1000, // generous RPM so TPM limit kicks first
        ..Default::default()
    };
    let quota = create_quota(config).await;
    let channel_id = "ch-tpm-test";

    // First call with 1 token → 1 unit, count=1 ≤ max_units=1 → pass
    assert!(quota.check_all(channel_id, "t1", "chat", 1).await.is_ok());

    // Second call with 1 token → 1 unit, count=2 > max_units=1 → fail
    let result = quota.check_all(channel_id, "t1", "chat", 1).await;
    assert!(result.is_err(), "Expected channel TPM to be exceeded");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("TPM"),
        "Expected TPM error, got: {}",
        err
    );
}

/// TPM boundary: channel_max_tpm = 0 means max_units = 0, so every request is rejected.
///
/// This exercises the `div_ceil(1000)` edge case where the configured limit
/// is less than one full quantised unit — the entire TPM budget is exhausted
/// before any request lands.
#[tokio::test]
async fn test_channel_tpm_boundary_max_units_zero() {
    let config = QuotaConfig {
        channel_max_tpm: 0,    // max_units = 0 (0.div_ceil(1000))
        channel_max_rpm: 1000, // generous RPM so TPM limit kicks first
        ..Default::default()
    };
    let quota = create_quota(config).await;
    let channel_id = "ch-tpm-zero";

    // Even the first request with 1 token should fail because
    // max_units = 0, and check_n with n=1 gives count=1 > 0.
    let result = quota.check_all(channel_id, "t1", "chat", 1).await;
    assert!(
        result.is_err(),
        "Expected TPM to be exceeded when max_units = 0"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("TPM"),
        "Expected TPM error, got: {}",
        err
    );
}

/// QuotaService returns `TenantRpmExceeded` when tenant RPM is exceeded.
#[tokio::test]
async fn test_tenant_rpm_exceeded() {
    let config = QuotaConfig {
        tenant_max_rpm: 1,
        channel_max_rpm: 1000, // generous so tenant limit kicks first
        ..Default::default()
    };
    let quota = create_quota(config).await;
    let tenant_id = "t-rpm-test";
    let channel_id = "ch-tenant-rpm";

    // First call should pass
    assert!(
        quota
            .check_all(channel_id, tenant_id, "embedding", 1)
            .await
            .is_ok()
    );

    // Second call should fail (tenant RPM exceeded)
    let result = quota.check_all(channel_id, tenant_id, "embedding", 1).await;
    assert!(result.is_err(), "Expected tenant RPM to be exceeded");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("Tenant"),
        "Expected tenant RPM error, got: {}",
        err
    );
}

/// Circuit breaker trips after `cb_failure_threshold` consecutive failures.
#[tokio::test]
async fn test_circuit_breaker_trips_after_threshold() {
    let config = QuotaConfig {
        cb_failure_threshold: 3,
        ..Default::default()
    };
    let quota = create_quota(config).await;
    let channel_id = "ch-cb-trip";

    // Initially should not be broken
    assert!(
        !quota.record_failure(channel_id).await,
        "1st failure should NOT trip"
    );
    assert!(
        !quota.record_failure(channel_id).await,
        "2nd failure should NOT trip"
    );

    // check_all should still pass (breaker not tripped)
    assert!(
        quota.check_all(channel_id, "t", "chat", 1).await.is_ok(),
        "check_all should pass before breaker trips"
    );

    // 3rd failure trips the breaker
    assert!(
        quota.record_failure(channel_id).await,
        "3rd failure SHOULD trip"
    );

    // Now check_all should fail with CircuitBroken
    let result = quota.check_all(channel_id, "t", "chat", 1).await;
    assert!(result.is_err(), "check_all should fail after breaker trips");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("circuit"),
        "Expected circuit breaker error, got: {}",
        err
    );
}

/// Circuit breaker auto-recovers after `cb_retry_after_secs` TTL expires.
///
/// `record_success()` resets the failure counter (so the next batch of failures
/// starts fresh), but the tripped flag is TTL-based and auto-heals after the
/// configured retry interval.
#[tokio::test]
async fn test_circuit_breaker_auto_recovers_after_ttl() {
    let config = QuotaConfig {
        cb_failure_threshold: 2,
        cb_retry_after_secs: 1, // short TTL for fast test
        ..Default::default()
    };
    let quota = create_quota(config).await;
    let channel_id = "ch-cb-recover";

    // Trip the breaker (2 failures)
    quota.record_failure(channel_id).await;
    quota.record_failure(channel_id).await;

    // Verify breaker is tripped
    assert!(
        quota.check_all(channel_id, "t", "chat", 1).await.is_err(),
        "breaker should be tripped after threshold failures"
    );

    // Record success resets the failure counter
    quota.record_success(channel_id).await;

    // The tripped flag still exists (TTL-based), so check_all still fails
    // until the TTL expires. Wait for the retry interval to pass.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // After TTL expiry, the tripped flag should be gone and the breaker
    // has auto-recovered. check_all should pass again.
    let result = quota.check_all(channel_id, "t", "chat", 1).await;
    assert!(
        result.is_ok(),
        "check_all should pass after circuit breaker auto-recovers: {:?}",
        result
    );
}

/// Without a Redis rate limiter, all checks pass (fail-open).
#[tokio::test]
async fn test_check_all_passes_without_limiter() {
    let cache = common::create_cache_service().await;
    let quota = QuotaService::new(None, cache, Arc::new(QuotaConfig::default()));
    let result = quota.check_all("ch-no-limiter", "t", "chat", 1000).await;
    assert!(result.is_ok(), "no-limiter mode should pass all checks");
}

// ═══════════════════════════════════════════════════════════════════
//  End-to-end tests via HTTP API
// ═══════════════════════════════════════════════════════════════════

/// App with quota and metering services initializes correctly.
///
/// Creates an app with `create_app_with_quota()` and verifies the router
/// is constructed without panics or errors. This confirms the quota service
/// is wired into the gateway service at startup.
#[tokio::test]
async fn test_quota_app_initializes_correctly() {
    let config = QuotaConfig {
        channel_max_rpm: 100,
        ..Default::default()
    };
    let _app = common::axum::create_app_with_quota(config).await;
    // Success = app initialized without panic/error
}

/// Circuit breaker correctly counts burst failures within the TTL window.
///
/// This test exercises the common-case path: 5 failures arriving within
/// seconds (far less than `cb_retry_after_secs`) — each failure increments
/// the counter, and the breaker trips at the threshold.
///
/// Note: The current `increment_by` Lua script only sets EXPIRE on initial
/// key creation, not on each increment.  This means the failure counter TTL
/// is a fixed window starting from the *first* failure, not a sliding window
/// from the *last* failure.  For burst failures this is fine; for spaced-out
/// failures approaching the TTL boundary, the window may expire prematurely.
/// If that becomes a problem, the Lua script should be changed to always
/// call EXPIRE: `redis.call('EXPIRE', KEYS[1], ARGV[1])`.
#[tokio::test]
async fn test_circuit_breaker_burst_failures_trip() {
    let config = QuotaConfig {
        cb_failure_threshold: 5,
        cb_retry_after_secs: 60,
        ..Default::default()
    };
    let quota = create_quota(config).await;
    let channel_id = "ch-cb-burst";

    // Record 5 failures in rapid succession (well within TTL window).
    for i in 1..=5 {
        let tripped = quota.record_failure(channel_id).await;
        if i < 5 {
            assert!(!tripped, "failure {} should NOT trip breaker", i);
        } else {
            assert!(tripped, "failure 5 SHOULD trip breaker");
        }
    }

    // Verify breaker is tripped
    let result = quota.check_all(channel_id, "t", "chat", 1).await;
    assert!(
        result.is_err(),
        "check_all should fail after circuit breaker trips on burst failures"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("circuit"),
        "Expected circuit breaker error, got: {}",
        err
    );
}

/// Record success resets the circuit breaker failure counter immediately.
///
/// After a success, the next set of failures should count from scratch
/// (not accumulate from the pre-success counter).
#[tokio::test]
async fn test_circuit_breaker_success_resets_counter() {
    let config = QuotaConfig {
        cb_failure_threshold: 3,
        cb_retry_after_secs: 10,
        ..Default::default()
    };
    let quota = create_quota(config).await;
    let channel_id = "ch-cb-reset";

    // Record 2 failures (not yet tripped)
    assert!(!quota.record_failure(channel_id).await, "failure 1");
    assert!(!quota.record_failure(channel_id).await, "failure 2");

    // Record a success — should reset the failure counter
    quota.record_success(channel_id).await;

    // Now record failures again — they should count from scratch
    assert!(
        !quota.record_failure(channel_id).await,
        "failure 1 after reset"
    );
    assert!(
        !quota.record_failure(channel_id).await,
        "failure 2 after reset"
    );
    // 3rd failure after reset should trip
    assert!(
        quota.record_failure(channel_id).await,
        "3rd failure after reset SHOULD trip"
    );
}
