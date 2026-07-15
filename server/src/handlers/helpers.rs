//! Handler helper functions — reduce boilerplate in handler implementations.
//!
//! # Why this module exists
//!
//! Every authenticated handler starts with the same two lines:
//! ```ignore
//! let state: AppState = req.get_data().ok_or_else(|| HttpError::internal("..."))?;
//! let auth_user: AuthUser = req.get_data().ok_or_else(|| HttpError::unauthorized("..."))?;
//! ```
//!
//! These helper functions centralise this pattern, reducing duplication
//! and making AI-generated handlers more concise.

use crate::AppState;
use crate::middlewares::AuthUser;
use crate::services::gateway::{GatewayError, GatewayService};
use crate::services::tenant::TenantService;
use ains_runtime::{HttpError, RequestContext, Response};

/// Extract `AppState` from a request context.
///
/// Intended for **public** (unauthenticated) handlers where only the
/// application state is needed.
pub fn extract_state(req: &crate::ServerRequest) -> Result<AppState, HttpError> {
    req.get_data()
        .ok_or_else(|| HttpError::internal("AppState not available"))
}

/// Extract `(AppState, AuthUser)` from a request context.
///
/// Intended for **authenticated** handlers. Returns:
/// - `HttpError::internal` if AppState is missing (should never happen at runtime)
/// - `HttpError::unauthorized` if AuthUser is missing (request did not pass auth middleware)
pub fn extract_handler_context(
    req: &crate::ServerRequest,
) -> Result<(AppState, AuthUser), HttpError> {
    let state: AppState = req
        .get_data()
        .ok_or_else(|| HttpError::internal("AppState not available"))?;
    let auth_user: AuthUser = req
        .get_data()
        .ok_or_else(|| HttpError::unauthorized("Authentication required"))?;
    Ok((state, auth_user))
}

/// Map a `GatewayError` to an `HttpError` for handler responses.
pub fn handle_gateway_error(e: GatewayError) -> HttpError {
    match e {
        GatewayError::NoChannel => {
            HttpError::service_unavailable("No active AI channel supports this capability")
        }
        GatewayError::NotFound => HttpError::not_found("Channel not found"),
        GatewayError::InvalidInput(s) => HttpError::bad_request(s),
        GatewayError::Upstream(e) => {
            tracing::warn!(upstream_error = %e, "AI provider request failed");
            HttpError::service_unavailable("AI provider request failed")
        }
        GatewayError::Internal(e) => {
            tracing::error!(error = ?e, "gateway operation failed");
            HttpError::internal("AI gateway operation failed")
        }
    }
}

/// Extract the `GatewayService` reference from the app state.
pub fn gateway_service(state: &AppState) -> &GatewayService {
    &state.gateway
}

/// Verify that the specified tenant is active (not disabled).
pub async fn require_active_tenant(state: &AppState, tenant_id: &str) -> Result<(), HttpError> {
    if !TenantService::with_cache(state.db.clone(), state.cache.clone())
        .is_active(tenant_id)
        .await
        .map_err(|_| HttpError::internal("Failed to verify tenant"))?
    {
        return Err(HttpError::forbidden("Tenant is disabled"));
    }
    Ok(())
}

/// Build a JSON response with status 200 OK.
pub fn json_response<T: serde::Serialize>(value: &T) -> Result<Response, HttpError> {
    Response::json(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;

    #[test]
    fn handle_gateway_error_no_channel() {
        let err = GatewayError::NoChannel;
        let http = handle_gateway_error(err);
        assert_eq!(http.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(http.message.contains("no active") || http.message.contains("No active"));
    }

    #[test]
    fn handle_gateway_error_not_found() {
        let err = GatewayError::NotFound;
        let http = handle_gateway_error(err);
        assert_eq!(http.status, StatusCode::NOT_FOUND);
        assert!(http.message.contains("not found") || http.message.contains("Not found"));
    }

    #[test]
    fn handle_gateway_error_invalid_input() {
        let err = GatewayError::InvalidInput("bad request".into());
        let http = handle_gateway_error(err);
        assert_eq!(http.status, StatusCode::BAD_REQUEST);
        assert_eq!(http.message, "bad request");
    }

    #[test]
    fn handle_gateway_error_upstream() {
        let err = GatewayError::Upstream("rate limited".into());
        let http = handle_gateway_error(err);
        assert_eq!(http.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(http.message.contains("provider"));
    }

    #[test]
    fn handle_gateway_error_internal() {
        let err = GatewayError::Internal(anyhow::anyhow!("db connection failed"));
        let http = handle_gateway_error(err);
        assert_eq!(http.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(http.message.contains("gateway operation"));
    }

    #[test]
    fn json_response_ok() {
        let data = serde_json::json!({"key": "value"});
        let result = json_response(&data);
        assert!(result.is_ok());
    }

    // NOTE: `require_active_tenant` and `gateway_service` cannot be unit
    // tested here because they depend on AppState which requires a live
    // PostgreSQL connection and Redis instance.  Their behaviour is
    // covered by integration tests in:
    //   - server/tests/axum_tenant_test.rs  (tenant status checks)
    //   - server/tests/axum_gateway_test.rs  (gateway channel CRUD)
    #[test]
    fn json_response_produces_valid_json() {
        let data = serde_json::json!({"message": "hello", "code": 200});
        let resp = json_response(&data).expect("json_response should succeed");
        let bytes = resp.read_bytes().expect("should have bytes body");
        let body_str = String::from_utf8_lossy(&bytes);
        assert!(body_str.contains("\"message\""));
        assert!(body_str.contains("\"hello\""));
        assert!(body_str.contains("\"code\":200"));
    }
}
