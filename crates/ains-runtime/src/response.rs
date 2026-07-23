use bytes::Bytes;
use cookie::Cookie;
use http::HeaderName;
use http::StatusCode;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc::Receiver;

use crate::HttpError;

/// Unified response type — cookies are stored as `set-cookie` headers.
pub struct Response {
    status: StatusCode,
    headers: Vec<(HeaderName, String)>,
    body: ResponseBody,
    /// Explicit Content-Type override (takes priority over auto-detection).
    content_type: Option<&'static str>,
}

pub enum ResponseBody {
    Empty,
    Json(JsonValue),
    Bytes(Bytes),
    /// Server-Sent Events streaming response.
    /// Each item is a complete SSE event string (as raw bytes).
    Sse(Receiver<Result<Bytes, String>>),
}

impl Response {
    pub fn new() -> Self {
        Self {
            status: StatusCode::OK,
            headers: Vec::new(),
            body: ResponseBody::Empty,
            content_type: None,
        }
    }

    pub fn with_status(status: StatusCode) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ResponseBody::Empty,
            content_type: None,
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn set_status(&mut self, status: StatusCode) {
        self.status = status;
    }

    pub fn set_json_body(&mut self, value: impl Serialize) {
        let json_value = serde_json::to_value(value).unwrap_or(JsonValue::Null);
        self.body = ResponseBody::Json(json_value);
    }

    pub fn set_bytes_body(&mut self, bytes: Bytes) {
        self.body = ResponseBody::Bytes(bytes);
    }

    pub fn set_text_body(&mut self, text: impl Into<String>) {
        let text: String = text.into();
        self.body = ResponseBody::Bytes(Bytes::from(text));
    }

    /// Set explicit Content-Type (overrides auto-detection).
    pub fn set_content_type(&mut self, content_type: &'static str) {
        self.content_type = Some(content_type);
    }

    /// Create a new SSE streaming response.
    pub fn sse(rx: Receiver<Result<Bytes, String>>) -> Self {
        let mut res = Self::new();
        res.body = ResponseBody::Sse(rx);
        res.content_type = Some("text/event-stream");
        res
    }

    /// Returns `true` if this is an SSE streaming response.
    pub fn is_sse(&self) -> bool {
        matches!(self.body, ResponseBody::Sse(_))
    }

    /// Take ownership of the SSE receiver.
    /// Returns `None` if this is not an SSE response.
    pub fn take_sse_receiver(&mut self) -> Option<Receiver<Result<Bytes, String>>> {
        let old = std::mem::replace(&mut self.body, ResponseBody::Empty);
        match old {
            ResponseBody::Sse(rx) => Some(rx),
            other => {
                self.body = other;
                None
            }
        }
    }

    /// Check if explicit Content-Type is set.
    pub fn content_type(&self) -> Option<&'static str> {
        self.content_type
    }

    pub fn insert_header(&mut self, name: &'static str, value: impl ToString) {
        if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
            self.headers.push((header_name, value.to_string()));
        }
    }

    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(h, _)| h.as_str() != name);
    }

    pub fn set_cookie(&mut self, cookie: Cookie<'static>) {
        self.insert_header("set-cookie", cookie.to_string());
    }

    pub fn remove_cookie(&mut self, name: &str) {
        let mut c = Cookie::new(name.to_owned(), "");
        c.set_path("/");
        c.set_max_age(cookie::time::Duration::seconds(0));
        c.set_http_only(true);
        self.insert_header("set-cookie", c.to_string());
    }

    pub fn read_bytes(&self) -> Result<Bytes, String> {
        match &self.body {
            ResponseBody::Empty => Ok(Bytes::new()),
            ResponseBody::Json(val) => serde_json::to_vec(val)
                .map(Bytes::from)
                .map_err(|e| e.to_string()),
            ResponseBody::Bytes(bytes) => Ok(bytes.clone()),
            ResponseBody::Sse(_) => {
                Err("Cannot read bytes from SSE streaming response".to_string())
            }
        }
    }

    pub fn take_headers(&mut self) -> Vec<(HeaderName, String)> {
        std::mem::take(&mut self.headers)
    }

    pub fn body(&self) -> &ResponseBody {
        &self.body
    }

    /// Build JSON response with status 200 OK.
    pub fn json<T: Serialize>(value: &T) -> Result<Self, HttpError> {
        let mut res = Self::new();
        res.set_json_body(
            serde_json::to_value(value).map_err(|e| HttpError::internal(e.to_string()))?,
        );
        Ok(res)
    }
}

impl Default for Response {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseBody {
    pub fn is_json(&self) -> bool {
        matches!(self, ResponseBody::Json(_))
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, ResponseBody::Empty)
    }
}

// ── From<HttpError> for Response ──
impl From<HttpError> for Response {
    fn from(err: HttpError) -> Self {
        let mut res = Response::new();
        res.set_status(err.status);
        res.set_json_body(serde_json::json!({
            "error": err.error_type,
            "message": err.message,
        }));
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_defaults_to_none() {
        let resp = Response::new();
        assert!(resp.content_type().is_none());
    }

    #[test]
    fn content_type_can_be_set_explicitly() {
        let mut resp = Response::new();
        resp.set_content_type("application/pdf");
        assert_eq!(resp.content_type(), Some("application/pdf"));
    }

    #[test]
    fn content_type_overrides_previous_value() {
        let mut resp = Response::new();
        resp.set_content_type("text/html");
        resp.set_content_type("application/json");
        assert_eq!(resp.content_type(), Some("application/json"));
    }

    #[test]
    fn with_status_defaults_content_type_to_none() {
        let resp = Response::with_status(StatusCode::NOT_FOUND);
        assert!(resp.content_type().is_none());
    }

    #[test]
    fn json_constructor_does_not_set_content_type() {
        let resp = Response::json(&serde_json::json!({"key": "value"})).unwrap();
        assert!(resp.content_type().is_none());
        assert!(resp.body().is_json());
    }

    #[test]
    fn from_http_error_does_not_set_content_type() {
        let err = HttpError::bad_request("test");
        let resp: Response = err.into();
        assert!(resp.content_type().is_none());
    }

    #[test]
    fn sse_constructor_sets_content_type() {
        let (_, rx) = tokio::sync::mpsc::channel(1);
        let resp = Response::sse(rx);
        assert!(resp.is_sse());
        assert_eq!(resp.content_type(), Some("text/event-stream"));
    }

    #[test]
    fn take_sse_receiver_returns_receiver() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut resp = Response::sse(rx);
        assert!(resp.is_sse());
        let taken = resp.take_sse_receiver();
        assert!(taken.is_some(), "should return the receiver");
        assert!(!resp.is_sse(), "should no longer be SSE after taking");
        // The channel is still usable (tx is still alive)
        let _ = tx.try_send(Ok(Bytes::from("data: hello\n\n")));
    }

    #[test]
    fn non_sse_take_receiver_returns_none() {
        let mut resp = Response::new();
        assert!(resp.take_sse_receiver().is_none());
        let mut resp = Response::with_status(StatusCode::OK);
        assert!(resp.take_sse_receiver().is_none());
    }
}
