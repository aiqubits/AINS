//! Framework-agnostic middleware types and re-exports from the active adapter.

pub(crate) const JWT_COOKIE: &str = "ains_jwt";
pub(crate) const REFRESH_COOKIE: &str = "ains_refresh";
pub(crate) const EXPIRY_COOKIE: &str = "ains_exp";

// Re-export AuthUser and RateLimitGuard from ains-runtime
pub use ains_runtime::{AuthUser, RateLimitGuard};

// Re-export generate_token for backward compatibility
pub use crate::utils::jwt::generate_token;

// Axum mode: re-export middleware from the adapter
#[cfg(not(feature = "ains-salvo"))]
pub use ains_axum::middleware::{
    auth_middleware, panic_middleware, rate_limit_middleware, require_admin,
};

// Salvo mode: re-export middleware from the adapter
#[cfg(feature = "ains-salvo")]
pub use ains_salvo::middleware::{AuthMiddleware, RateLimitMiddleware, RequireAdmin};
