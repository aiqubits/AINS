//! Integration tests for the distributed lock service with real Redis.
//!
//! Unlike the unit test in `services/lock.rs` which is `#[ignore]` and uses a
//! hardcoded Redis URL, these tests use the shared test configuration to connect
//! to the Redis instance managed by the test harness.

use ains_server::services::lock::{AcquireResult, LockGuard, acquire_lock, release_lock};
use std::time::Duration;

mod common;

/// Acquire and release a distributed lock successfully.
#[tokio::test]
async fn test_acquire_and_release_lock() {
    let config = common::load_test_config();
    let client = redis::Client::open(config.redis_url.as_str()).unwrap();
    let lock_key = format!("test:lock:ar_{}", common::unique_table_name("lk"));

    let (acquired, lock_value) =
        acquire_lock(Some(&client), &lock_key, 10, 1, Duration::from_millis(100))
            .await
            .expect("acquire_lock should succeed");
    assert!(acquired, "should acquire the lock");

    release_lock(Some(&client), &lock_key, &lock_value)
        .await
        .expect("release_lock should succeed");
}

/// Second concurrent acquire for the same key should fail.
#[tokio::test]
async fn test_lock_contention_is_rejected() {
    let config = common::load_test_config();
    let client = redis::Client::open(config.redis_url.as_str()).unwrap();
    let lock_key = format!("test:lock:cnt_{}", common::unique_table_name("lk"));

    let (acquired, value) =
        acquire_lock(Some(&client), &lock_key, 10, 1, Duration::from_millis(100))
            .await
            .expect("first acquire_lock should succeed");
    assert!(acquired, "first lock should be acquired");

    let (acquired2, _) = acquire_lock(Some(&client), &lock_key, 10, 1, Duration::from_millis(100))
        .await
        .expect("second acquire_lock should not error");
    assert!(!acquired2, "second concurrent lock should be rejected");

    release_lock(Some(&client), &lock_key, &value)
        .await
        .expect("release_lock should succeed");
}

/// Lock with a 1-second TTL auto-releases after expiry.
#[tokio::test]
async fn test_lock_auto_releases_after_ttl() {
    let config = common::load_test_config();
    let client = redis::Client::open(config.redis_url.as_str()).unwrap();
    let lock_key = format!("test:lock:ttl_{}", common::unique_table_name("lk"));

    // Acquire lock with 1-second TTL (expiry_seconds=1)
    let (acquired, _value) =
        acquire_lock(Some(&client), &lock_key, 1, 1, Duration::from_millis(100))
            .await
            .expect("acquire_lock should succeed");
    assert!(acquired, "should acquire the lock");

    // Wait for TTL to expire
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Now a new acquire should succeed (lock auto-released by TTL)
    let (acquired2, value2) =
        acquire_lock(Some(&client), &lock_key, 1, 1, Duration::from_millis(100))
            .await
            .expect("second acquire_lock should succeed after TTL");
    assert!(acquired2, "should re-acquire after TTL expiry");

    // Clean up
    release_lock(Some(&client), &lock_key, &value2).await.ok();
}

/// Releasing a lock with wrong value is a no-op (safety check).
#[tokio::test]
async fn test_release_with_wrong_value_is_noop() {
    let config = common::load_test_config();
    let client = redis::Client::open(config.redis_url.as_str()).unwrap();
    let lock_key = format!("test:lock:wv_{}", common::unique_table_name("lk"));

    let (acquired, _value) =
        acquire_lock(Some(&client), &lock_key, 1, 1, Duration::from_millis(100))
            .await
            .expect("acquire_lock should succeed");
    assert!(acquired, "should acquire the lock");

    // Try to release with wrong value — must NOT actually release
    release_lock(Some(&client), &lock_key, "wrong-value")
        .await
        .expect("release with wrong value should not error");

    // Lock should still be held
    let (acquired2, _) = acquire_lock(Some(&client), &lock_key, 1, 1, Duration::from_millis(100))
        .await
        .expect("acquire_lock should still succeed");
    assert!(!acquired2, "lock still held by original value");

    // Wait for TTL and then clean up
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (acquired3, value3) =
        acquire_lock(Some(&client), &lock_key, 1, 1, Duration::from_millis(100))
            .await
            .expect("acquire_lock should succeed after TTL");
    assert!(acquired3, "should re-acquire after TTL expiry");
    release_lock(Some(&client), &lock_key, &value3).await.ok();
}

/// No Redis client = acquire_lock returns an error (fail-close).
#[tokio::test]
async fn test_acquire_without_redis_returns_error() {
    let result = acquire_lock(None, "test:lock:nr", 10, 1, Duration::from_millis(100)).await;
    assert!(result.is_err(), "acquire_lock without Redis should error");
}

/// LockGuard correctly detects contention vs acquisition.
#[tokio::test]
async fn test_lock_guard_correctly_reports_contention() {
    let config = common::load_test_config();
    let client = redis::Client::open(config.redis_url.as_str()).unwrap();
    let lock_key = format!("test:lock:gc_{}", common::unique_table_name("lk"));

    // First acquire should get Acquired
    let guard = LockGuard::acquire(Some(&client), &lock_key, 10, 1, Duration::from_millis(100))
        .await
        .expect("LockGuard::acquire should not error");
    match guard {
        Some(AcquireResult::Acquired(_)) => {} // expected
        Some(AcquireResult::Contended) => panic!("first acquire should not be contended"),
        None => panic!("should not be None"),
    }

    // Second acquire should get Contended
    let guard2 = LockGuard::acquire(Some(&client), &lock_key, 10, 1, Duration::from_millis(100))
        .await
        .expect("LockGuard::acquire should not error");
    match guard2 {
        Some(AcquireResult::Contended) => {} // expected
        Some(AcquireResult::Acquired(_)) => panic!("second acquire should be contended"),
        None => panic!("should not be None"),
    }

    // Drop the first guard to release the lock
    drop(guard);

    // Allow the async drop handler to complete
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should be able to acquire after drop
    let (acquired, value) =
        acquire_lock(Some(&client), &lock_key, 10, 1, Duration::from_millis(100))
            .await
            .expect("acquire after drop should succeed");
    assert!(acquired, "should acquire after guard release");

    release_lock(Some(&client), &lock_key, &value).await.ok();
}
