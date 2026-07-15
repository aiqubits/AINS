use std::sync::{Arc, Mutex};

use redis::Script;
use redis::aio::ConnectionManager;

use crate::config::RateLimitConfig;
use crate::error::RateLimitError;

/// Redis‑backed fixed‑window rate limiter.
///
/// Uses a Lua script to atomically `INCR` + `EXPIRE` so that even if the
/// Redis connection fails between the two commands, the key never
/// survives without a TTL (which would cause permanent rate limiting).
///
/// The `ConnectionManager` is lazily initialised on the first `check()`
/// call and cached behind an `Arc<Mutex<...>>` so that all clones share
/// the same underlying Redis connection.
pub struct RedisRateLimiter {
    client: Option<redis::Client>,
    /// Lazily‑initialised Redis connection manager.  An `Arc` ensures all
    /// clones share the same connection pool rather than each creating
    /// its own.
    conn_manager: Arc<Mutex<Option<ConnectionManager>>>,
    config: RateLimitConfig,
}

impl RedisRateLimiter {
    /// Create an active rate limiter backed by the given `redis::Client`.
    pub fn new(client: redis::Client, config: RateLimitConfig) -> Self {
        Self {
            client: Some(client),
            conn_manager: Arc::new(Mutex::new(None)),
            config,
        }
    }

    /// Create a disabled rate limiter that always allows requests.
    ///
    /// Useful when Redis is not configured (development / no‑Redis fallback).
    pub fn disabled(config: RateLimitConfig) -> Self {
        Self {
            client: None,
            conn_manager: Arc::new(Mutex::new(None)),
            config,
        }
    }

    // ── Accessors ────────────────────────────────────────────────────────

    /// Whether a Redis backend is available.
    pub fn is_available(&self) -> bool {
        self.client.is_some()
    }

    /// Whether this limiter should fail‑open (allow request) when Redis is
    /// unreachable.
    pub fn fail_open(&self) -> bool {
        self.config.fail_open
    }

    /// Immutable reference to the configuration.
    pub fn config(&self) -> &RateLimitConfig {
        &self.config
    }

    // ── Core check ───────────────────────────────────────────────────────

    /// Check **and** increment the rate limit counter for `key`.
    ///
    /// - `key` – the bare key (e.g. `"login:ip:1.2.3.4"`). The configured
    ///   `key_prefix` is prepended automatically.
    /// - `max_requests` – how many requests are allowed within the window.
    /// - `window_seconds` – the length of the fixed window in seconds.
    ///
    /// Returns:
    /// - `Ok(true)` – request is allowed (counter ≤ `max_requests`).
    /// - `Ok(false)` – rate limit exceeded.
    /// - `Err(RateLimitError)` – Redis unavailable or command failed.
    ///
    ///   Atomic rate‑limit check: increment counter and set expiry in one
    ///   Redis call via a Lua script, eliminating the INCR / EXPIRE race.
    ///
    /// See https://redis.io/docs/latest/develop/interact/programmability/eval-intro/
    /// for the scripting semantics used here.
    /// Check **and** increment by `n` the rate limit counter for `key`.
    ///
    /// Same as [`check`](Self::check) but atomically adds `n` to the counter
    /// in a single Redis call.  This avoids the partial-budget-consumption
    /// problem that would arise from looping `n` times over `check()`.
    ///
    /// - `n` – how much to increment (e.g. token units).  Must be > 0.
    pub async fn check_n(
        &self,
        key: &str,
        max_requests: u64,
        window_seconds: u64,
        n: u64,
    ) -> Result<bool, RateLimitError> {
        debug_assert!(n > 0, "check_n: n must be positive");
        let mut conn = self.conn().await?;
        let full_key = self.build_key(key);

        // Lua script: atomically INCRBY, set TTL on first creation,
        // return the new count.
        //
        // KEYS[1] – the counter key
        // ARGV[1] – TTL in seconds
        // ARGV[2] – increment amount (n)
        const INCRBY_AND_EXPIRE: &str = r#"
            local count = redis.call('INCRBY', KEYS[1], ARGV[2])
            if count == tonumber(ARGV[2]) then
                redis.call('EXPIRE', KEYS[1], ARGV[1])
            end
            return count
        "#;

        let count: u64 = Script::new(INCRBY_AND_EXPIRE)
            .key(&full_key)
            .arg(window_seconds)
            .arg(n)
            .invoke_async(&mut conn)
            .await?;

        Ok(count <= max_requests)
    }

    /// Check **and** increment the rate limit counter for `key`.
    ///
    /// - `key` – the bare key (e.g. `"login:ip:1.2.3.4"`). The configured
    ///   `key_prefix` is prepended automatically.
    /// - `max_requests` – how many requests are allowed within the window.
    /// - `window_seconds` – the length of the fixed window in seconds.
    ///
    /// Returns:
    /// - `Ok(true)` – request is allowed (counter ≤ `max_requests`).
    /// - `Ok(false)` – rate limit exceeded.
    /// - `Err(RateLimitError)` – Redis unavailable or command failed.
    ///
    ///   Atomic rate‑limit check: increment counter and set expiry in one
    ///   Redis call via a Lua script, eliminating the INCR / EXPIRE race.
    ///
    /// See https://redis.io/docs/latest/develop/interact/programmability/eval-intro/
    /// for the scripting semantics used here.
    pub async fn check(
        &self,
        key: &str,
        max_requests: u64,
        window_seconds: u64,
    ) -> Result<bool, RateLimitError> {
        let mut conn = self.conn().await?;

        let full_key = self.build_key(key);

        // Lua script that atomically increments a counter and sets its
        // TTL on first creation.
        //
        // KEYS[1] – the counter key
        // ARGV[1] – TTL in seconds
        const INC_AND_EXPIRE: &str = r#"
            local count = redis.call('INCR', KEYS[1])
            if count == 1 then
                redis.call('EXPIRE', KEYS[1], ARGV[1])
            end
            return count
        "#;

        let count: u64 = Script::new(INC_AND_EXPIRE)
            .key(&full_key)
            .arg(window_seconds)
            .invoke_async(&mut conn)
            .await?;

        Ok(count <= max_requests)
    }

    // ── Internals ────────────────────────────────────────────────────────

    fn build_key(&self, bare: &str) -> String {
        if self.config.key_prefix.is_empty() {
            bare.to_string()
        } else {
            format!("{}:{}", self.config.key_prefix, bare)
        }
    }

    /// Return a `ConnectionManager`, **caching** it after the first call.
    ///
    /// Uses double‑checked locking so that the (expensive) `get_connection_manager()`
    /// is called only once.  The lock is never held across an `.await` point.
    async fn conn(&self) -> Result<ConnectionManager, RateLimitError> {
        let client = self.client.as_ref().ok_or(RateLimitError::NotAvailable)?;

        // Fast path: manager already initialised (no lock held across await).
        {
            let guard = self
                .conn_manager
                .lock()
                .expect("rate limit connection manager mutex poisoned (fast path)");
            if let Some(ref cm) = *guard {
                return Ok(cm.clone());
            }
        }

        // Slow path: initialise the manager.
        let cm = client
            .get_connection_manager()
            .await
            .map_err(RateLimitError::Redis)?;

        // Store (lock is never held across await).
        let mut guard = self
            .conn_manager
            .lock()
            .expect("rate limit connection manager mutex poisoned (store path)");
        // Double‑check: another task might have initialised while we were awaiting.
        if guard.is_none() {
            *guard = Some(cm.clone());
        }
        Ok(cm)
    }
}

// Manual `Debug` impl because `ConnectionManager` doesn't implement `Debug`.
impl std::fmt::Debug for RedisRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisRateLimiter")
            .field("client", &self.client.as_ref().map(|_| "<redis client>"))
            .field(
                "conn_manager",
                &self
                    .conn_manager
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|_| "<connection manager>")),
            )
            .field("config", &self.config)
            .finish()
    }
}

// Manual `Clone` impl because `Mutex` is not `Clone`.
// `Arc` and `Option<redis::Client>` are both `Clone`, so this is straightforward.
impl Clone for RedisRateLimiter {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            conn_manager: Arc::clone(&self.conn_manager),
            config: self.config.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_key_with_prefix() {
        let limiter = RedisRateLimiter::disabled(RateLimitConfig {
            key_prefix: "ratelimit".to_string(),
            fail_open: true,
        });
        assert_eq!(
            limiter.build_key("login:ip:1.2.3.4"),
            "ratelimit:login:ip:1.2.3.4"
        );
    }

    #[test]
    fn test_build_key_empty_prefix() {
        let limiter = RedisRateLimiter::disabled(RateLimitConfig {
            key_prefix: String::new(),
            fail_open: true,
        });
        assert_eq!(limiter.build_key("some:key"), "some:key");
    }

    #[test]
    fn test_is_available() {
        let disabled = RedisRateLimiter::disabled(RateLimitConfig::default());
        assert!(!disabled.is_available());
    }

    #[test]
    fn test_default_config() {
        let cfg = RateLimitConfig::default();
        assert_eq!(cfg.key_prefix, "ratelimit");
        assert!(cfg.fail_open);
    }

    #[tokio::test]
    async fn test_check_disabled_returns_not_available() {
        let limiter = RedisRateLimiter::disabled(RateLimitConfig::default());
        let result = limiter.check("test:key", 0, 60).await;
        assert!(
            matches!(result, Err(RateLimitError::NotAvailable)),
            "disabled limiter should return NotAvailable, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_check_disabled_regardless_of_limit() {
        let limiter = RedisRateLimiter::disabled(RateLimitConfig::default());
        // Even with max_requests=0, disabled limiter should fail
        // (NotAvailable) rather than rate-limiting.
        let result = limiter.check("another:key", 0, 60).await;
        assert!(result.is_err(), "disabled limiter must not contact Redis");
    }

    // ── check_n tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_check_n_disabled_returns_not_available() {
        let limiter = RedisRateLimiter::disabled(RateLimitConfig::default());
        let result = limiter.check_n("test:key", 10, 60, 3).await;
        assert!(
            matches!(result, Err(RateLimitError::NotAvailable)),
            "disabled limiter should return NotAvailable for check_n, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_check_n_disabled_regardless_of_limit() {
        let limiter = RedisRateLimiter::disabled(RateLimitConfig::default());
        // Even with max_requests=0, n=0, disabled limiter must fail with
        // NotAvailable rather than contacting Redis or returning a pass.
        let result = limiter.check_n("another:key", 0, 60, 1).await;
        assert!(result.is_err(), "disabled limiter must not contact Redis");
    }

    // NOTE: Full Redis integration tests for check_n (INCRBY atomicity,
    // TTL behaviour, multi-increment rejection) require a running Redis
    // instance.  They are covered at the integration-test level in:
    //   - server/tests/axum_ratelimit_test.rs  (per-IP rate limiting)
    //   - server/tests/axum_quota_test.rs       (channel/tenant quota checks)
    // which use check_n internally through the quota service.

    #[tokio::test]
    #[ignore]
    async fn test_check_n_zero_n_is_noop() {
        // check_n with n = 0 must not crash or panic, and it should return
        // Ok(true) when the count is within the limit (since INCRBY 0 does
        // not change the counter).
        // Requires a running Redis instance at 127.0.0.1:6379.
        let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
        let limiter = RedisRateLimiter::new(client, RateLimitConfig::default());
        let key = "test:check_n_zero";
        let full_key = limiter.build_key(key);
        let mut conn = limiter.conn().await.unwrap();

        // Ensure clean state
        let _: () = redis::cmd("DEL")
            .arg(&full_key)
            .query_async(&mut conn)
            .await
            .unwrap();

        // n = 0 should pass when under limit
        let result = limiter.check_n(key, 10, 60, 0).await;
        assert!(
            matches!(result, Ok(true)),
            "check_n with n=0 should return Ok(true) when under limit, got {:?}",
            result
        );

        // The counter should still be 0 after n=0 INCRBY
        let val: Option<u64> = redis::cmd("GET")
            .arg(&full_key)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(
            val.unwrap_or(0) == 0,
            "counter should still be 0 after n=0 check_n, got {:?}",
            val
        );

        // Clean up
        let _: () = redis::cmd("DEL")
            .arg(&full_key)
            .query_async(&mut conn)
            .await
            .unwrap();
    }
}
