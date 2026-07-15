//! Unified `/api/ai/chat` handler — OpenAI Responses API protocol.
//!
//! This handler replaces the old Chat Completions handler completely.
//! It accepts the Responses API request format, translates to upstream
//! Chat Completions protocol, proxies the request, and translates the
//! response back to Responses API format.
//!
//! Supports both non-streaming (JSON) and streaming (SSE) modes.

use crate::handlers::helpers;
use crate::services::MeteringService;
use crate::services::responses::{
    ResponsesRequest, detect_capability, sse_events, translate_request, translate_response,
    translate_streaming_chunk,
};
use ains_runtime::{HttpError, RequestContext, Response};
use bytes::Bytes;
use serde_json::Value;

fn error(e: crate::services::gateway::GatewayError) -> HttpError {
    helpers::handle_gateway_error(e)
}

fn service(state: &crate::AppState) -> &crate::services::gateway::GatewayService {
    helpers::gateway_service(state)
}

async fn require_active_tenant(state: &crate::AppState, tenant_id: &str) -> Result<(), HttpError> {
    helpers::require_active_tenant(state, tenant_id).await
}

// ── Main handler ────────────────────────────────────────────────

pub async fn responses_chat(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = helpers::extract_handler_context(&req)?;

    // Tenant check
    if actor.role != "system" {
        require_active_tenant(&state, &actor.tenant_id).await?;
    }

    // Parse request body as Responses API format
    let body: Value = req.parse_json().await.map_err(HttpError::bad_request)?;
    let responses_req: ResponsesRequest = serde_json::from_value(body.clone())
        .map_err(|e| HttpError::bad_request(format!("Invalid Responses API request: {}", e)))?;

    // Detect required capabilities from the request
    let capabilities = detect_capability(&responses_req);

    // For now, use the primary capability (Chat) for routing.
    // Multi-capability requests (e.g., chat + vision) are handled by
    // the channel selection which checks capabilities.
    let primary_capability = capabilities
        .first()
        .cloned()
        .unwrap_or(crate::repositories::channel::ModelCapability::Chat);

    // Translate the request to upstream Chat Completions format
    let upstream_body = translate_request(&responses_req)
        .map_err(|e| HttpError::bad_request(format!("Request translation error: {}", e)))?;

    // Check streaming mode
    let is_streaming = responses_req.stream.unwrap_or(false);

    if is_streaming {
        handle_streaming(&state, &actor, primary_capability, upstream_body).await
    } else {
        handle_non_streaming(
            &state,
            &actor,
            primary_capability,
            upstream_body,
            &responses_req,
        )
        .await
    }
}

// ── Non-streaming handler ────────────────────────────────────────

async fn handle_non_streaming(
    state: &crate::AppState,
    actor: &ains_runtime::AuthUser,
    capability: crate::repositories::channel::ModelCapability,
    upstream_body: Value,
    _req: &ResponsesRequest,
) -> Result<Response, HttpError> {
    // Proxy via existing non-streaming method
    let result = service(state)
        .proxy(&actor.tenant_id, &actor.user_id, capability, upstream_body)
        .await
        .map_err(error)?;

    // Translate upstream response to Responses API format
    let model = result
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");

    let responses_response = translate_response(&result, model);

    Response::json(&responses_response)
}

// ── Streaming handler ────────────────────────────────────────────

async fn handle_streaming(
    state: &crate::AppState,
    actor: &ains_runtime::AuthUser,
    capability: crate::repositories::channel::ModelCapability,
    upstream_body: Value,
) -> Result<Response, HttpError> {
    // Start streaming proxy — this creates a background task that reads
    // streaming chunks from the upstream and sends them through a channel.
    // The returned channel_id identifies which channel was selected for
    // downstream token metering recording.
    let (mut upstream_rx, channel_id) = service(state)
        .proxy_stream(&actor.tenant_id, &actor.user_id, capability, upstream_body)
        .await
        .map_err(error)?;

    // Prepare metering dependencies before entering the spawned task.
    // The spawned closure cannot borrow `state` or `actor`.
    let metering = MeteringService::new(state.db.clone());
    let meter_tenant_id = actor.tenant_id.clone();
    let meter_user_id = actor.user_id.clone();

    // Create a channel for the final SSE events that go to the client
    let (sse_tx, sse_rx) = tokio::sync::mpsc::unbounded_channel();

    // Spawn a task that reads from the upstream stream, translates events,
    // and sends them through the SSE channel.
    // A keepalive interval is also included to prevent proxy/load-balancer
    // timeouts on long-running SSE connections with infrequent data.
    //
    // MAX_SSE_BUFFER: safety limit to prevent unbounded memory growth when
    // the upstream sends data without SSE event terminators (\n\n).
    const MAX_SSE_BUFFER: usize = 1_048_576; // 1 MB
    tokio::spawn(async move {
        let mut text_accumulator = String::new();
        let mut usage_input: u64 = 0;
        let mut usage_output: u64 = 0;
        let mut model_name: Option<String> = None;
        let mut consecutive_parse_errors: u32 = 0;

        // Buffer for accumulating partial SSE events across chunks
        let mut buffer = String::new();
        let mut keepalive_interval = tokio::time::interval(std::time::Duration::from_secs(15));
        // The first tick completes immediately; consume it so the first
        // keepalive fires after 15s instead of almost immediately.
        keepalive_interval.tick().await;

        loop {
            tokio::select! {
                chunk_result = upstream_rx.recv() => {
                    let chunk = match chunk_result {
                        Some(c) => c,
                        None => break, // Stream ended
                    };
                    match chunk {
                        Ok(bytes) => {
                            if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                                // Safety guard: prevent unbounded buffer growth when upstream
                                // sends data without \n\n event terminators (adversarial/buggy).
                                if buffer.len() + text.len() > MAX_SSE_BUFFER {
                                    tracing::error!(
                                        buffer_bytes = buffer.len() + text.len(),
                                        max = MAX_SSE_BUFFER,
                                        "SSE buffer overflow — terminating stream"
                                    );
                                    let _ = sse_tx.send(Err(
                                        "Internal error: stream buffer overflow".into()
                                    ));
                                    return;
                                }
                                buffer.push_str(&text);
                                while let Some(pos) = buffer.find("\n\n") {
                                    let event_str = buffer[..pos].to_string();
                                    buffer = buffer[pos + 2..].to_string();
                                    match process_upstream_event(
                                        &event_str, &mut text_accumulator,
                                        &mut model_name,
                                        &mut usage_input, &mut usage_output,
                                        &sse_tx,
                                    ) {
                                        EventResult::Continue => {
                                            consecutive_parse_errors = 0;
                                        }
                                        EventResult::ParseError => {
                                            consecutive_parse_errors += 1;
                                            if consecutive_parse_errors >= MAX_CONSECUTIVE_PARSE_ERRORS {
                                                tracing::error!(
                                                    errors = consecutive_parse_errors,
                                                    "Too many consecutive SSE parse errors — terminating stream"
                                                );
                                                let _ = sse_tx.send(Err(
                                                    "Internal error: upstream data format error".into()
                                                ));
                                                return;
                                            }
                                        }
                                        EventResult::ClientDisconnected => {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let error_event = sse_events::error_event("stream_error", &e);
                            let error_str = sse_events::format_sse_event("error", &error_event);
                            let _ = sse_tx.send(Ok(Bytes::from(error_str)));
                            return;
                        }
                    }
                }
                _ = keepalive_interval.tick() => {
                    // Send SSE keepalive comment to prevent connection timeout
                    if sse_tx.send(Ok(Bytes::from(": keepalive\n\n"))).is_err() {
                        return; // Client disconnected
                    }
                }
            }
        }

        // Extract model name from upstream response if available.
        // The last upstream chunk may contain model/usage info.
        // For OpenAI streaming, the final usage chunk includes model.
        // For Anthropic streaming, model is in message_start event.
        let final_model = model_name.unwrap_or_else(|| "unknown".to_string());

        // Estimate token usage from accumulated text if upstream didn't provide it
        if usage_input == 0 && usage_output == 0 {
            // Rough estimate: ~4 chars per token
            let estimated = (text_accumulator.len() as u64 / 4).max(1);
            usage_output = estimated;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));

        let response_payload = serde_json::json!({
            "id": format!("resp_{}", uuid::Uuid::new_v4().to_string().replace('-', "")),
            "object": "response",
            "created_at": now,
            "model": final_model,
            "output": [{
                "id": msg_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text_accumulator}]
            }],
            "usage": {
                "input_tokens": usage_input,
                "output_tokens": usage_output,
                "total_tokens": usage_input + usage_output
            }
        });
        // Record token metering for the completed streaming response.
        // This is best-effort: recording failures must not affect the SSE output.
        let uid: i64 = meter_user_id.parse().unwrap_or_else(|e| {
            tracing::error!(
                error = %e,
                raw_user_id = %meter_user_id,
                tenant_id = %meter_tenant_id,
                channel_id = %channel_id,
                "Failed to parse user_id as i64 — metering will use 0"
            );
            0
        });
        let _ = metering
            .record_usage(
                uid,
                &meter_tenant_id,
                channel_id,
                &final_model,
                "chat",
                &response_payload,
            )
            .await;

        let completed_event =
            serde_json::json!({"type": "response.completed", "response": response_payload});
        let completed_str = sse_events::format_sse_event("response.completed", &completed_event);
        let _ = sse_tx.send(Ok(Bytes::from(completed_str)));
    });

    Ok(Response::sse(sse_rx))
}

/// The result of processing an upstream SSE event.
enum EventResult {
    /// Event processed successfully, continue streaming.
    Continue,
    /// Client disconnected, stop streaming.
    ClientDisconnected,
    /// Parse error occurred. If consecutive errors exceed the threshold,
    /// the caller should terminate the stream.
    ParseError,
}

/// Maximum consecutive upstream parse errors before terminating the stream.
/// Prevents silent data loss when upstream sends persistently malformed data.
const MAX_CONSECUTIVE_PARSE_ERRORS: u32 = 10;

/// Process a single upstream SSE event string and forward translated events.
fn process_upstream_event(
    event_str: &str,
    text_accumulator: &mut String,
    model_name: &mut Option<String>,
    usage_input: &mut u64,
    usage_output: &mut u64,
    sse_tx: &tokio::sync::mpsc::UnboundedSender<Result<Bytes, String>>,
) -> EventResult {
    // Skip empty lines and comments
    if event_str.is_empty() || event_str.starts_with(':') {
        return EventResult::Continue;
    }

    // Extract data from SSE event (data: {...})
    let data_line = event_str
        .lines()
        .find(|line| line.starts_with("data: "))
        .and_then(|line| line.strip_prefix("data: "));

    let data = match data_line {
        Some(d) => d.trim(),
        None => return EventResult::Continue,
    };

    // Skip [DONE] signal
    if data == "[DONE]" {
        return EventResult::Continue;
    }

    // Parse JSON with error logging
    let chunk: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "Failed to parse upstream SSE event as JSON: {}. Data preview: {:.100}",
                e,
                data
            );
            return EventResult::ParseError;
        }
    };

    // Extract and translate delta — this also populates model_name and usage
    let result = translate_streaming_chunk(
        &chunk,
        text_accumulator,
        model_name,
        usage_input,
        usage_output,
    );

    if let Some((sse_str, _delta)) = result {
        // If client disconnected (send fails), signal caller to stop
        if sse_tx.send(Ok(Bytes::from(sse_str))).is_err() {
            return EventResult::ClientDisconnected;
        }
    }

    EventResult::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normal SSE event with a text delta — should be forwarded.
    #[test]
    fn process_upstream_event_normal_delta() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let event =
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let result = process_upstream_event(event, &mut acc, &mut model, &mut inp, &mut out, &tx);
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(!acc.is_empty(), "accumulator should contain delta");
        assert!(rx.try_recv().is_ok(), "should have sent an SSE event");
    }

    /// Empty event string — should be skipped silently.
    #[test]
    fn process_upstream_event_empty_string() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let result = process_upstream_event("", &mut acc, &mut model, &mut inp, &mut out, &tx);
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(acc.is_empty());
        assert!(_rx.is_empty());
    }

    /// Comment line (starts with :) — should be skipped.
    #[test]
    fn process_upstream_event_comment_line() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let result =
            process_upstream_event(": keepalive", &mut acc, &mut model, &mut inp, &mut out, &tx);
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(_rx.is_empty());
    }

    /// [DONE] signal — should be skipped (no event sent).
    #[test]
    fn process_upstream_event_done_signal() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let result = process_upstream_event(
            "data: [DONE]",
            &mut acc,
            &mut model,
            &mut inp,
            &mut out,
            &tx,
        );
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(_rx.is_empty());
    }

    /// Malformed JSON in the data field — should return ParseError.
    #[test]
    fn process_upstream_event_malformed_json() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let result = process_upstream_event(
            "data: {invalid json!!!",
            &mut acc,
            &mut model,
            &mut inp,
            &mut out,
            &tx,
        );
        drop(tx);

        assert!(matches!(result, EventResult::ParseError));
        assert!(_rx.is_empty());
    }

    /// Event with no "data:" line — should be skipped.
    #[test]
    fn process_upstream_event_no_data_line() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let result = process_upstream_event(
            "event: ping\necho: something",
            &mut acc,
            &mut model,
            &mut inp,
            &mut out,
            &tx,
        );
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(_rx.is_empty());
    }

    /// Client disconnects (receiver dropped) — should return ClientDisconnected.
    #[test]
    fn process_upstream_event_client_disconnected() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        drop(rx); // Drop receiver before sending

        let event =
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let result = process_upstream_event(event, &mut acc, &mut model, &mut inp, &mut out, &tx);

        assert!(matches!(result, EventResult::ClientDisconnected));
    }

    /// Event with usage data updates input/output counters.
    #[test]
    fn process_upstream_event_with_usage_updates_counters() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let event = r#"data: {"usage":{"prompt_tokens":10,"completion_tokens":20},"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let result = process_upstream_event(event, &mut acc, &mut model, &mut inp, &mut out, &tx);
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert_eq!(inp, 10);
        assert_eq!(out, 20);
    }
}
