//! AI Gateway quota management and circuit breaking.
//!
//! Provides channel-level and tenant-level rate limiting on top of the existing
//! user-level Redis rate limiting, plus a simple circuit breaker that prevents
//! requests from reaching a channel after consecutive upstream failures.
//!
//! # Graceful degradation
//!
//! When Redis is unavailable, all quota checks silently pass (fail-open) so that
//! an interrupted Redis connection never blocks AI proxy requests.

use std::sync::Arc;
use std::time::Duration;

use distributed_ratelimit::RedisRateLimiter;

use crate::services::CacheService;
use crate::utils::config::QuotaConfig;

#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error("Channel rate limit exceeded (RPM)")]
    ChannelRpmExceeded,
    #[error("Channel rate limit exceeded (TPM)")]
    ChannelTpmExceeded,
    #[error("Tenant rate limit exceeded")]
    TenantRpmExceeded,
    #[error("Channel circuit is broken (too many failures)")]
    CircuitBroken,
    #[error("Rate limiter unavailable")]
    Unavailable,
}

/// AI Gateway quota and circuit breaker service.
///
/// Holds an optional `RedisRateLimiter` (shared from the application's rate
/// limiter infrastructure) and a reference to `CacheService` for circuit
/// breaker failure counting.
///
/// When Redis is not available, all checks pass (fail-open).
#[derive(Clone)]
pub struct QuotaService {
    /// Rate limiter for RPM/TPM checks. `None` means all checks pass.
    limiter: Option<RedisRateLimiter>,
    /// Cache service for circuit breaker failure counters.
    cache: CacheService,
    /// Quota configuration.
    config: Arc<QuotaConfig>,
}

impl QuotaService {
    /// Create a new quota service.
    ///
    /// Pass `limiter: None` to disable all rate limiting (graceful degradation).
    pub fn new(
        limiter: Option<RedisRateLimiter>,
        cache: CacheService,
        config: Arc<QuotaConfig>,
    ) -> Self {
        Self {
            limiter,
            cache,
            config,
        }
    }

    /// Check all quotas before proxying to a channel.
    ///
    /// Returns `Ok(())` if all checks pass, or the first `QuotaError` that fails.
    pub async fn check_all(
        &self,
        channel_id: &str,
        tenant_id: &str,
        capability: &str,
        estimated_tokens: u64,
    ) -> Result<(), QuotaError> {
        // 1. Circuit breaker — check before rate limits (fastest check).
        if self.is_circuit_broken(channel_id).await? {
            return Err(QuotaError::CircuitBroken);
        }

        // 2. Channel RPM check.
        if !self.check_channel_rpm(channel_id).await? {
            return Err(QuotaError::ChannelRpmExceeded);
        }

        // 3. Channel TPM check.
        if !self.check_channel_tpm(channel_id, estimated_tokens).await? {
            return Err(QuotaError::ChannelTpmExceeded);
        }

        // 4. Tenant RPM check.
        if !self.check_tenant_rpm(tenant_id, capability).await? {
            return Err(QuotaError::TenantRpmExceeded);
        }

        Ok(())
    }

    /// Record a successful upstream proxy call — resets the circuit breaker
    /// failure counter for this channel.
    ///
    /// Clears both the failure counter AND the tripped flag so that the
    /// circuit breaker can self-heal after a successful request.
    pub async fn record_success(&self, channel_id: &str) {
        let failures_key = format!("cb:channel:{}:failures", channel_id);
        let tripped_key = format!("cb:channel:{}:tripped", channel_id);
        let _ = self.cache.invalidate(&failures_key).await;
        let _ = self.cache.invalidate(&tripped_key).await;
    }

    /// Record an upstream failure — atomically increments the circuit breaker counter.
    ///
    /// Returns `true` if the circuit has now tripped (i.e. the failure threshold
    /// was just exceeded).
    pub async fn record_failure(&self, channel_id: &str) -> bool {
        let counter_key = format!("cb:channel:{}:failures", channel_id);
        let tripped_key = format!("cb:channel:{}:tripped", channel_id);
        let ttl = Duration::from_secs(self.config.cb_retry_after_secs);

        // Atomically increment the failure counter (Redis Lua script).
        // This eliminates the TOCTOU race of read → increment → write.
        let new_count: u64 = self
            .cache
            .increment_by(&counter_key, 1, ttl)
            .await
            .unwrap_or(1); // fail-safe: treat as first failure on cache error

        if new_count >= self.config.cb_failure_threshold as u64 {
            // Trip the circuit — set a TTL-based key.
            let _ = self.cache.set(&tripped_key, &true, ttl).await;
            tracing::warn!(
                channel_id = %channel_id,
                failures = new_count,
                retry_after_secs = self.config.cb_retry_after_secs,
                "Circuit breaker tripped for channel"
            );
            true
        } else {
            false
        }
    }

    /// Check whether a channel's circuit breaker is currently tripped.
    ///
    /// Exposed so the gateway can filter open channels out of weighted
    /// selection instead of randomly picking one and failing the request.
    pub async fn is_circuit_broken(&self, channel_id: &str) -> Result<bool, QuotaError> {
        let key = format!("cb:channel:{}:tripped", channel_id);
        Ok(self.cache.exists(&key).await.unwrap_or(false))
    }

    /// Check channel-level requests-per-minute limit.
    async fn check_channel_rpm(&self, channel_id: &str) -> Result<bool, QuotaError> {
        let Some(ref limiter) = self.limiter else {
            return Ok(true);
        };
        let key = format!("quota:channel:{}:rpm", channel_id);
        match limiter.check(&key, self.config.channel_max_rpm, 60).await {
            Ok(result) => Ok(result),
            Err(e) => {
                if self.config.fail_open {
                    tracing::warn!(
                        "Redis rate limiter unavailable — failing open for channel RPM check: {}",
                        e
                    );
                }
                Ok(self.config.fail_open)
            }
        }
    }

    /// Check channel-level tokens-per-minute limit.
    ///
    /// Uses per-token costing: treats each `estimated_tokens` as a "request"
    /// to the rate limiter, effectively rate-limiting the *sum of tokens*.
    /// This is an approximation: a single 100K-token request may be counted
    /// as 100 "hits", while the real upstream TPM limit might allow it.
    /// For exact tracking, use token-level metering in the proxy response.
    ///
    /// Uses the atomic `check_n` API to avoid partial budget consumption:
    /// the old loop-based approach incremented the counter one unit at a
    /// time, so a request that exceeded the remaining budget would consume
    /// some budget before being rejected.
    async fn check_channel_tpm(
        &self,
        channel_id: &str,
        estimated_tokens: u64,
    ) -> Result<bool, QuotaError> {
        let Some(ref limiter) = self.limiter else {
            return Ok(true);
        };
        if estimated_tokens == 0 {
            return Ok(true);
        }
        // Compute how many "units" this request costs in the TPM window.
        // Each "unit" = 1000 tokens, rounded up.
        let units = estimated_tokens.div_ceil(1000);
        let key = format!("quota:channel:{}:tpm", channel_id);
        // The max_requests is channel_max_tpm / 1000 so that the counter
        // reflects the token budget.
        let max_units = self.config.channel_max_tpm.div_ceil(1000);
        // Atomically consume all units in a single Redis call.
        match limiter.check_n(&key, max_units, 60, units).await {
            Ok(result) => Ok(result),
            Err(e) => {
                if self.config.fail_open {
                    tracing::warn!(
                        "Redis rate limiter unavailable — failing open for channel TPM check: {}",
                        e
                    );
                }
                Ok(self.config.fail_open)
            }
        }
    }

    /// Check tenant-level requests-per-minute limit per capability.
    async fn check_tenant_rpm(
        &self,
        tenant_id: &str,
        capability: &str,
    ) -> Result<bool, QuotaError> {
        let Some(ref limiter) = self.limiter else {
            return Ok(true);
        };
        let key = format!("quota:tenant:{}:{}:rpm", tenant_id, capability);
        match limiter.check(&key, self.config.tenant_max_rpm, 60).await {
            Ok(result) => Ok(result),
            Err(e) => {
                if self.config.fail_open {
                    tracing::warn!(
                        "Redis rate limiter unavailable — failing open for tenant RPM check: {}",
                        e
                    );
                }
                Ok(self.config.fail_open)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn noop_cache() -> CacheService {
        // Empty URL creates a no-op cache (no Redis connection).
        CacheService::new("", 0).await
    }

    #[tokio::test]
    async fn check_all_passes_without_redis() {
        // All checks should pass even when Redis is unavailable (fail-open).
        let config = Arc::new(QuotaConfig::default());
        let quota = QuotaService::new(None, noop_cache().await, config);
        let result = quota.check_all("ch1", "t1", "chat", 100).await;
        assert!(result.is_ok(), "no-Redis mode should pass all checks");
    }

    #[tokio::test]
    #[ignore = "requires Redis (circuit breaker state persistence)"]
    async fn circuit_breaker_trips_after_threshold() {
        let config = Arc::new(QuotaConfig {
            cb_failure_threshold: 3,
            cb_retry_after_secs: 60,
            ..Default::default()
        });
        let quota = QuotaService::new(None, noop_cache().await, config.clone());

        // First 2 failures should NOT trip
        assert!(!quota.record_failure("ch-test").await);
        assert!(!quota.record_failure("ch-test").await);

        // Breaker should NOT be broken yet
        assert!(!quota.is_circuit_broken("ch-test").await.unwrap());

        // 3rd failure should trip
        assert!(quota.record_failure("ch-test").await);

        // Breaker should be broken
        assert!(quota.is_circuit_broken("ch-test").await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires Redis (circuit breaker state persistence)"]
    async fn record_success_resets_breaker() {
        let config = Arc::new(QuotaConfig {
            cb_failure_threshold: 2,
            cb_retry_after_secs: 60,
            ..Default::default()
        });
        let quota = QuotaService::new(None, noop_cache().await, config);

        // Two failures to trip
        quota.record_failure("ch-reset").await;
        quota.record_failure("ch-reset").await;
        assert!(quota.is_circuit_broken("ch-reset").await.unwrap());

        // Record success — should reset
        quota.record_success("ch-reset").await;
        assert!(!quota.is_circuit_broken("ch-reset").await.unwrap());
    }

    #[test]
    fn quota_error_display() {
        assert_eq!(
            QuotaError::ChannelRpmExceeded.to_string(),
            "Channel rate limit exceeded (RPM)"
        );
        assert_eq!(
            QuotaError::CircuitBroken.to_string(),
            "Channel circuit is broken (too many failures)"
        );
    }
}
