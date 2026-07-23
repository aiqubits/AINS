use http::StatusCode;
use serde_json::Value;

/// Unified HTTP error type with JSON-compatible format.
#[derive(Debug)]
pub struct HttpError {
    pub status: StatusCode,
    pub error_type: &'static str,
    pub message: String,
}

impl HttpError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error_type: "bad_request",
            message: msg.into(),
        }
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            error_type: "unauthorized",
            message: msg.into(),
        }
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            error_type: "forbidden",
            message: msg.into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error_type: "not_found",
            message: msg.into(),
        }
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error_type: "conflict",
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error_type: "internal_error",
            message: msg.into(),
        }
    }

    pub fn service_unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "service_unavailable",
            message: msg.into(),
        }
    }

    pub fn too_many_requests(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            error_type: "rate_limited",
            message: msg.into(),
        }
    }

    /// Build an error with an explicit status code.
    ///
    /// Useful when proxying an upstream status (e.g. a provider 413/422) that
    /// does not map onto one of the named constructors. Falls back to
    /// `400 Bad Request` when `status` is not a valid HTTP status code.
    pub fn with_status(status: u16, error_type: &'static str, msg: impl Into<String>) -> Self {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
        Self {
            status,
            error_type,
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {}: {}",
            self.status.as_u16(),
            self.error_type,
            self.message
        )
    }
}

impl std::error::Error for HttpError {}

/// Build the stable error envelope used by POST /api/ai/response.
///
/// Authentication middleware uses this before the request reaches the
/// endpoint handler, so the client sees one schema for every failure path.
pub fn ai_response_error_body(error_type: &str, message: &str) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    serde_json::json!({
        "id": format!("resp_{:x}", now.as_nanos()),
        "object": "response",
        "created_at": now.as_secs() as i64,
        "model": Value::Null,
        "capability": Value::Null,
        "status": "failed",
        "incomplete_details": Value::Null,
        "output": [],
        "usage": Value::Null,
        "error": {
            "code": error_type,
            "message": message
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_response_errors_have_a_stable_failed_envelope() {
        let body = ai_response_error_body("unauthorized", "Authentication required");
        assert!(
            body["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("resp_"))
        );
        assert_eq!(body["object"], "response");
        assert_eq!(body["status"], "failed");
        assert_eq!(body["incomplete_details"], Value::Null);
        assert_eq!(body["output"], serde_json::json!([]));
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(body["error"].get("type").is_none());
        assert!(body["error"].get("param").is_none());
    }
}
