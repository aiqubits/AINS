//! OpenAI Responses API protocol types and bidirectional translation.
//!
//! This module provides:
//! - Rust types matching the OpenAI `/v1/responses` API format
//! - Request translation: Responses API → Chat Completions (upstream)
//! - Response translation: Chat Completions (upstream) → Responses API
//! - Streaming event translation: upstream SSE → Responses SSE events
//!
//! Architecture: "protocol translation mode" — the AINS server accepts
//! Responses API format on `/api/ai/chat` and translates to/from upstream
//! Chat Completions / Anthropic Messages protocols.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════
//  Request Types
// ═══════════════════════════════════════════════════════════════════

/// Top-level request for POST /api/ai/chat (Responses API format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    /// Model identifier (e.g. "gpt-4o", "claude-3-opus").
    /// When unspecified, the server selects from available channels.
    #[serde(default)]
    pub model: Option<String>,

    /// The input to the model: a plain string or an array of input items.
    pub input: Input,

    /// System-level instructions (equivalent to system role in Chat Completions).
    /// Maps to the first system message in the upstream request.
    #[serde(default)]
    pub instructions: Option<String>,

    /// Developer-level instructions (software logic constraints).
    /// Maps to a developer role message following the system message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,

    /// Maximum number of output tokens to generate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,

    /// Whether to stream the response using SSE.
    #[serde(default)]
    pub stream: Option<bool>,

    /// Tools for the model to use (e.g. web_search).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolConfig>>,

    /// Whether to store the response. Defaults to true.
    /// Set to false for stateless/ZDR compliance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,

    /// ID of a previous response to continue from (multi-turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,

    /// Optional metadata key-value pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

/// The input to the model: either a plain string or an array of messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Input {
    /// Plain text string input (short form).
    Text(String),
    /// Array of input message items (long form, supports multi-modal).
    Messages(Vec<InputMessage>),
}

/// A single input message item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputMessage {
    /// Message role: "user", "assistant", "system", or "developer".
    pub role: String,
    /// Message content: text string or array of content parts.
    #[serde(default)]
    pub content: Content,
}

/// Content of a message: either a plain string or an array of content parts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    /// Plain text content (short form).
    Text(String),
    /// Array of typed content parts (supports multi-modal).
    Parts(Vec<ContentPart>),
}

impl Default for Content {
    fn default() -> Self {
        Content::Text(String::new())
    }
}

/// A single content part within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Text content.
    InputText { text: String },
    /// Image content (URL or base64 data).
    InputImage {
        /// The image URL or data URI.
        image_url: String,
        /// Optional detail level: "auto", "low", "high".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Audio content (base64 encoded).
    InputAudio {
        /// Base64-encoded audio data.
        data: String,
        /// Audio format (e.g. "wav", "mp3").
        format: String,
    },
    /// File content (e.g. PDF).
    InputFile {
        /// Base64-encoded file data.
        file_data: String,
        /// File name with extension.
        filename: String,
    },
}

/// Tool configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolConfig {
    /// Built-in web search tool.
    WebSearch {
        /// Optional: specific search query override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        /// Optional: search context size ("low", "medium", "high").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search_context_size: Option<String>,
    },
    /// File search tool.
    #[serde(rename = "file_search")]
    FileSearch {
        /// Vector store IDs to search.
        #[serde(default)]
        vector_store_ids: Vec<String>,
    },
}

impl Content {
    /// Returns the plain text content if this is a Text variant.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text(s) => Some(s.as_str()),
            Content::Parts(_) => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Response Types
// ═══════════════════════════════════════════════════════════════════

/// Top-level response (non-streaming).
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesResponse {
    /// Unique response ID (prefixed with "resp_").
    pub id: String,
    /// Object type, always "response".
    #[serde(rename = "object")]
    pub object: String,
    /// Unix timestamp of creation.
    pub created_at: i64,
    /// Model used to generate the response.
    pub model: String,
    /// Array of output items (messages, tool calls, etc.).
    pub output: Vec<OutputItem>,
    /// Token usage information.
    pub usage: Option<UsageResponse>,
}

/// A single output item in the response.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    /// A text message output.
    Message {
        /// Output item ID.
        id: String,
        /// Item status: "completed" or "incomplete".
        status: String,
        /// The content of the message.
        content: Vec<OutputContent>,
        /// The role of the message author ("assistant").
        role: String,
    },
}

/// Content within an output item.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    /// Text output.
    OutputText {
        /// The generated text.
        text: String,
        /// Annotations (citations, etc.).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<Annotation>,
    },
    /// Audio output (from TTS).
    #[serde(rename = "audio")]
    OutputAudio {
        /// Audio item ID.
        id: String,
        /// Base64-encoded audio data.
        data: String,
        /// Optional transcript of the audio.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript: Option<String>,
    },
    /// Image output.
    #[serde(rename = "image")]
    OutputImage {
        /// The image URL or data URI.
        image_url: String,
    },
}

/// An annotation on output text (citation, URL, etc.).
#[derive(Debug, Clone, Serialize)]
pub struct Annotation {
    /// The type of annotation.
    pub r#type: String,
    /// The annotated text.
    pub text: String,
    /// Index of the annotation in the text.
    pub start_index: u32,
    /// End index of the annotation.
    pub end_index: u32,
}

/// Token usage information.
#[derive(Debug, Clone, Serialize)]
pub struct UsageResponse {
    /// Number of input (prompt) tokens.
    pub input_tokens: u32,
    /// Number of output (completion) tokens.
    pub output_tokens: u32,
    /// Total tokens (input + output).
    pub total_tokens: u32,
}

// ═══════════════════════════════════════════════════════════════════
//  Request Translation: Responses → Chat Completions
// ═══════════════════════════════════════════════════════════════════

/// Translate a Responses API request into an upstream Chat Completions request body.
///
/// This is the core protocol translation function. It handles:
/// - Merging `instructions` and `developer_instructions` into the messages array
/// - Converting multi-modal content parts (image/audio/file) to upstream format
/// - Mapping Responses API parameter names to Chat Completions equivalents
pub fn translate_request(req: &ResponsesRequest) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::new();

    // 1. Instructions → system message (first in the array)
    if let Some(ref instructions) = req.instructions {
        messages.push(serde_json::json!({
            "role": "system",
            "content": instructions
        }));
    }

    // 2. Developer instructions → developer message (follows system)
    if let Some(ref dev_instructions) = req.developer_instructions {
        messages.push(serde_json::json!({
            "role": "developer",
            "content": dev_instructions
        }));
    }

    // 3. Input → user/assistant/system messages
    match &req.input {
        Input::Text(text) => {
            messages.push(serde_json::json!({
                "role": "user",
                "content": text
            }));
        }
        Input::Messages(items) => {
            for item in items {
                let content_value = translate_content(&item.content, &item.role)?;
                messages.push(serde_json::json!({
                    "role": item.role,
                    "content": content_value
                }));
            }
        }
    }

    // Build the chat completions request
    let mut upstream = serde_json::json!({
        "messages": messages
    });

    // Model override
    if let Some(ref model) = req.model {
        upstream["model"] = serde_json::json!(model);
    }

    // Max tokens
    if let Some(max_tokens) = req.max_output_tokens {
        upstream["max_tokens"] = serde_json::json!(max_tokens);
    }

    // Streaming
    if req.stream.unwrap_or(false) {
        upstream["stream"] = serde_json::json!(true);
    }

    // Tools → function definitions
    if let Some(ref tools) = req.tools {
        let functions: Vec<Value> = tools.iter().map(translate_tool).collect();
        if !functions.is_empty() {
            upstream["tools"] = serde_json::json!(functions);
        }
    }

    Ok(upstream)
}

/// Translate content parts (including multi-modal) to upstream format.
fn translate_content(content: &Content, role: &str) -> Result<Value, String> {
    match content {
        Content::Text(text) => Ok(serde_json::json!(text)),
        Content::Parts(parts) => {
            let translated: Vec<Value> = parts
                .iter()
                .map(|part| translate_content_part(part, role))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(serde_json::json!(translated))
        }
    }
}

/// Translate a single content part to upstream format.
fn translate_content_part(part: &ContentPart, _role: &str) -> Result<Value, String> {
    match part {
        ContentPart::InputText { text } => Ok(serde_json::json!({
            "type": "text",
            "text": text
        })),
        ContentPart::InputImage { image_url, detail } => {
            let mut part = serde_json::json!({
                "type": "image_url",
                "image_url": {
                    "url": image_url
                }
            });
            if let Some(d) = detail {
                part["image_url"]["detail"] = serde_json::json!(d);
            }
            Ok(part)
        }
        ContentPart::InputAudio { .. } => {
            // Audio input requires STT preprocessing before proxying.
            // This content part should be pre-processed by the handler.
            // Return a placeholder that signals the handler to run STT.
            Err("Audio content must be pre-processed via STT before proxying".to_string())
        }
        ContentPart::InputFile {
            file_data,
            filename,
        } => {
            // File/PDF input requires text extraction before proxying.
            // This content part should be pre-processed by the handler.
            Ok(serde_json::json!({
                "type": "text",
                "text": format!("[File attachment: {}. File data (base64, {} bytes) available for processing.]",
                    filename,
                    file_data.len())
            }))
        }
    }
}

/// Translate a tool configuration to upstream format.
fn translate_tool(tool: &ToolConfig) -> Value {
    match tool {
        ToolConfig::WebSearch { .. } => {
            // WebSearch is handled as a built-in capability routing decision,
            // not as a function call. For upstream, we inject a system prompt
            // instructing the model to use web knowledge when appropriate.
            // The actual web search capability is determined by channel routing.
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "web_search",
                    "description": "Search the web for current information",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "The search query"
                            }
                        },
                        "required": ["query"]
                    }
                }
            })
        }
        ToolConfig::FileSearch { vector_store_ids } => {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "file_search",
                    "description": "Search files in vector stores",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "The search query"
                            }
                        },
                        "required": ["query"]
                    },
                    "vector_store_ids": vector_store_ids
                }
            })
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Response Translation: Chat Completions → Responses
// ═══════════════════════════════════════════════════════════════════

/// Translate an upstream Chat Completions response into Responses API format.
pub fn translate_response(upstream: &Value, model: &str) -> ResponsesResponse {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let response_id = format!("resp_{}", Uuid::new_v4().to_string().replace('-', ""));

    // Extract the message from choices[0]
    let (output_text, output_role, finish_reason) = extract_choice_info(upstream);

    // Build output items
    let mut output = Vec::new();

    // Build the assistant message output item
    let mut content_items = Vec::new();
    if !output_text.is_empty() {
        content_items.push(OutputContent::OutputText {
            text: output_text,
            annotations: Vec::new(),
        });
    }

    let msg_id = format!("msg_{}", Uuid::new_v4().to_string().replace('-', ""));
    output.push(OutputItem::Message {
        id: msg_id,
        status: if finish_reason == "stop" || finish_reason == "length" {
            "completed".to_string()
        } else {
            "incomplete".to_string()
        },
        content: content_items,
        role: output_role,
    });

    // Extract usage
    let usage = extract_usage(upstream);

    ResponsesResponse {
        id: response_id,
        object: "response".to_string(),
        created_at: now,
        model: model.to_string(),
        output,
        usage,
    }
}

/// Extract choice information from an upstream response.
fn extract_choice_info(upstream: &Value) -> (String, String, String) {
    let default_msg = serde_json::json!({"role": "assistant", "content": ""});
    let choice = upstream
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .unwrap_or(&default_msg);

    let message = choice.get("message").unwrap_or(&default_msg);
    let text = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let role = message
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant")
        .to_string();
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|r| r.as_str())
        .unwrap_or("stop")
        .to_string();

    (text, role, finish_reason)
}

/// Extract token usage from an upstream response.
fn extract_usage(upstream: &Value) -> Option<UsageResponse> {
    let usage = upstream.get("usage")?;
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    // ChatGPT / upstream may use "completion_tokens" or we compute from Anthropic format.
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    // Or use total_tokens directly if available
    let total_tokens = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or((input_tokens + output_tokens) as u64) as u32;

    Some(UsageResponse {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}

// ═══════════════════════════════════════════════════════════════════
//  Streaming Event Translation
// ═══════════════════════════════════════════════════════════════════

/// SSE events for the Responses API streaming protocol.
pub mod sse_events {
    use super::*;

    /// Build a `response.output_text.delta` event payload.
    pub fn output_text_delta(delta: &str, index: usize) -> Value {
        serde_json::json!({
            "type": "response.output_text.delta",
            "delta": delta,
            "index": index
        })
    }

    /// Build a `response.completed` event payload with the full response.
    pub fn response_completed(response: &ResponsesResponse) -> Value {
        serde_json::json!({
            "type": "response.completed",
            "response": response
        })
    }

    /// Build an `error` event payload.
    pub fn error_event(code: &str, message: &str) -> Value {
        serde_json::json!({
            "type": "error",
            "code": code,
            "message": message
        })
    }

    /// Format a responses SSE event as a string suitable for the SSE stream.
    pub fn format_sse_event(event_type: &str, data: &Value) -> String {
        let json_str = serde_json::to_string(data).unwrap_or_else(|e| {
            tracing::error!("Failed to serialize SSE event data: {}", e);
            "{}".to_string()
        });
        format!("event: {}\ndata: {}\n\n", event_type, json_str)
    }
}

/// Extracts text delta from an upstream streaming chunk.
pub fn extract_upstream_delta(chunk: &Value) -> Option<String> {
    let choices = chunk.get("choices")?.as_array()?;
    let choice = choices.first()?;
    let delta = choice.get("delta")?;
    let content = delta.get("content")?.as_str()?;
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

/// Checks if an upstream chunk is the final [DONE] signal.
pub fn is_upstream_done(chunk: &Value) -> bool {
    chunk.as_str() == Some("[DONE]")
}

/// Translate a single upstream streaming chunk into optional Responses SSE event strings.
/// Returns None if the chunk should be filtered (e.g., no content delta).
/// Also extracts `model` and `usage` from the chunk as side effects.
pub fn translate_streaming_chunk(
    chunk: &Value,
    accumulated_text: &mut String,
    model_name: &mut Option<String>,
    usage_input: &mut u64,
    usage_output: &mut u64,
) -> Option<(String, String)> {
    // Check for [DONE] signal
    if is_upstream_done(chunk) {
        return None;
    }

    // Extract model from chunk if available (present in first upstream chunk)
    if model_name.is_none()
        && let Some(m) = chunk.get("model").and_then(|m| m.as_str())
        && !m.is_empty()
    {
        *model_name = Some(m.to_string());
    }

    // Extract usage if available (present in final chunk with usage summary)
    if let Some(usage) = chunk.get("usage") {
        if let Some(input) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
            *usage_input = input;
        }
        if let Some(output) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
            *usage_output = output;
        }
    }

    // Extract text delta
    if let Some(delta) = extract_upstream_delta(chunk) {
        accumulated_text.push_str(&delta);

        // Build delta event — index is always 0 for single text output
        let event_data = sse_events::output_text_delta(&delta, 0);
        let sse_str = sse_events::format_sse_event("response.output_text.delta", &event_data);
        return Some((sse_str, delta));
    }

    None
}

/// Counts tokens in text for usage estimation when upstream doesn't provide usage.
#[allow(dead_code)]
pub fn estimate_tokens_from_text(text: &str) -> u64 {
    (text.len() as u64 / 4).max(1)
}

// ═══════════════════════════════════════════════════════════════════
//  Anthropic Protocol Translation
// ═══════════════════════════════════════════════════════════════════

/// Convert a Chat Completions format request body into Anthropic Messages format.
///
/// Key differences:
/// - Anthropic puts `system` at the top level (not as a message)
/// - Anthropic doesn't support `developer` role — convert to system or prepend
/// - Anthropic uses `stop_reason` instead of `finish_reason`
pub fn chat_completions_to_anthropic(body: &Value) -> Value {
    let mut anthropic = serde_json::json!({});

    // Copy top-level fields that are shared
    if let Some(max_tokens) = body.get("max_tokens") {
        anthropic["max_tokens"] = max_tokens.clone();
    }
    if body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        anthropic["stream"] = serde_json::json!(true);
    }
    if let Some(stop) = body.get("stop") {
        // Anthropic expects stop_sequences as an array, but OpenAI accepts string or array
        match stop {
            Value::String(s) => {
                anthropic["stop_sequences"] = serde_json::json!([s]);
            }
            Value::Array(arr) => {
                anthropic["stop_sequences"] = Value::Array(arr.clone());
            }
            _ => {}
        }
    }
    if let Some(temp) = body.get("temperature") {
        anthropic["temperature"] = temp.clone();
    }
    if let Some(top_p) = body.get("top_p") {
        anthropic["top_p"] = top_p.clone();
    }

    // Extract messages and system prompt from the messages array
    let mut messages: Vec<Value> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg
                .get("content")
                .cloned()
                .unwrap_or(Value::String(String::new()));

            match role {
                "system" | "developer" => {
                    // System/developer instructions go to top-level `system` field
                    // Handle both string content and array content (multi-part)
                    match &content {
                        Value::String(text) => {
                            system_parts.push(text.clone());
                        }
                        Value::Array(parts) => {
                            for part in parts {
                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                    system_parts.push(text.to_string());
                                }
                            }
                        }
                        _ => {
                            tracing::warn!(
                                "Unexpected content type for system/developer message: {}",
                                serde_json::to_string(&content).unwrap_or_default()
                            );
                        }
                    }
                }
                _ => {
                    messages.push(serde_json::json!({
                        "role": role,
                        "content": content
                    }));
                }
            }
        }
    }

    if !system_parts.is_empty() {
        anthropic["system"] = Value::String(system_parts.join("\n"));
    }

    anthropic["messages"] = Value::Array(messages);

    anthropic
}

/// Convert an Anthropic Messages format response into Chat Completions format.
/// This allows the existing `translate_response()` pipeline to work unchanged.
pub fn anthropic_response_to_chat_completions(response: &Value) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Extract content text from Anthropic format
    let content_text = response
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();

    let role = response
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant");

    let stop_reason = response.get("stop_reason").and_then(|r| r.as_str());
    let finish_reason = match stop_reason {
        Some("end_turn" | "stop_sequence") => "stop",
        Some("max_tokens") => "length",
        _ => "stop",
    };

    let model = response
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");

    // Extract usage
    let (prompt_tokens, completion_tokens) = response
        .get("usage")
        .map(|usage| {
            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (input, output)
        })
        .unwrap_or((0, 0));

    serde_json::json!({
        "id": response.get("id").cloned().unwrap_or(Value::String("unknown".into())),
        "object": "chat.completion",
        "created": now,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": role,
                "content": content_text
            },
            "finish_reason": finish_reason
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    })
}

/// Extract text delta from an Anthropic SSE content_block_delta event.
pub fn extract_anthropic_streaming_delta(chunk: &Value) -> Option<String> {
    let event_type = chunk.get("type")?.as_str()?;
    match event_type {
        "content_block_delta" => {
            let delta = chunk.get("delta")?;
            let delta_type = delta.get("type")?.as_str()?;
            if delta_type == "text_delta" {
                delta.get("text")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Extract token usage from an Anthropic SSE message_delta event.
pub fn extract_anthropic_usage(chunk: &Value) -> Option<(u64, u64)> {
    let event_type = chunk.get("type")?.as_str()?;
    if event_type != "message_delta" {
        return None;
    }
    let usage = chunk.get("usage")?;
    let input = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some((input, output))
}

// ═══════════════════════════════════════════════════════════════════
//  Utility: Model/Channel resolution
// ═══════════════════════════════════════════════════════════════════

/// Determine the required capability based on content parts and tools.
pub fn detect_capability(
    req: &ResponsesRequest,
) -> Vec<crate::repositories::channel::ModelCapability> {
    use crate::repositories::channel::ModelCapability;

    let mut capabilities = vec![ModelCapability::Chat];

    // Check for image content
    if let Input::Messages(items) = &req.input {
        for item in items {
            if let Content::Parts(parts) = &item.content {
                for part in parts {
                    match part {
                        ContentPart::InputImage { .. }
                            if !capabilities.contains(&ModelCapability::Vision) =>
                        {
                            capabilities.push(ModelCapability::Vision);
                        }
                        ContentPart::InputAudio { .. }
                            if !capabilities.contains(&ModelCapability::Stt) =>
                        {
                            capabilities.push(ModelCapability::Stt);
                        }
                        ContentPart::InputFile { .. } => {
                            // File input stays as Chat (text extraction is needed)
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Check for tools
    if let Some(ref tools) = req.tools {
        for tool in tools {
            match tool {
                ToolConfig::WebSearch { .. } => {
                    if !capabilities.contains(&ModelCapability::WebSearch) {
                        capabilities.push(ModelCapability::WebSearch);
                    }
                }
                ToolConfig::FileSearch { .. } => {
                    // File search is handled at the application layer
                }
            }
        }
    }

    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Request Translation Tests ─────────────────────────────────

    #[test]
    fn translate_simple_text_input() {
        let req = ResponsesRequest {
            model: Some("gpt-4o".into()),
            input: Input::Text("Hello!".into()),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: None,
            stream: None,
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };

        let upstream = translate_request(&req).unwrap();
        let messages = upstream["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hello!");
        assert_eq!(upstream["model"], "gpt-4o");
        assert!(upstream.get("stream").is_none());
    }

    #[test]
    fn translate_instructions_to_system_message() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Text("What is Rust?".into()),
            instructions: Some("You are a helpful Rust expert.".into()),
            developer_instructions: None,
            max_output_tokens: None,
            stream: None,
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };

        let upstream = translate_request(&req).unwrap();
        let messages = upstream["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a helpful Rust expert.");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn translate_developer_instructions() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Text("Hello".into()),
            instructions: Some("You are an assistant.".into()),
            developer_instructions: Some("Always respond in JSON.".into()),
            max_output_tokens: None,
            stream: None,
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };

        let upstream = translate_request(&req).unwrap();
        let messages = upstream["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "developer");
        assert_eq!(messages[1]["content"], "Always respond in JSON.");
        assert_eq!(messages[2]["role"], "user");
    }

    #[test]
    fn translate_max_output_tokens() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Text("Hi".into()),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: Some(2048),
            stream: None,
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };

        let upstream = translate_request(&req).unwrap();
        assert_eq!(upstream["max_tokens"], 2048);
    }

    #[test]
    fn translate_stream_true() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Text("Hi".into()),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: None,
            stream: Some(true),
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };

        let upstream = translate_request(&req).unwrap();
        assert_eq!(upstream["stream"], true);
    }

    #[test]
    fn translate_multi_turn_messages() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Messages(vec![
                InputMessage {
                    role: "user".into(),
                    content: Content::Text("What's the capital of France?".into()),
                },
                InputMessage {
                    role: "assistant".into(),
                    content: Content::Text("Paris.".into()),
                },
                InputMessage {
                    role: "user".into(),
                    content: Content::Text("And its population?".into()),
                },
            ]),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: None,
            stream: None,
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };

        let upstream = translate_request(&req).unwrap();
        let messages = upstream["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[1]["content"], "Paris.");
    }

    #[test]
    fn translate_image_content() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Messages(vec![InputMessage {
                role: "user".into(),
                content: Content::Parts(vec![
                    ContentPart::InputText {
                        text: "What's in this image?".into(),
                    },
                    ContentPart::InputImage {
                        image_url: "https://example.com/photo.jpg".into(),
                        detail: Some("high".into()),
                    },
                ]),
            }]),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: None,
            stream: None,
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };

        let upstream = translate_request(&req).unwrap();
        let messages = upstream["messages"].as_array().unwrap();
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "https://example.com/photo.jpg"
        );
        assert_eq!(content[1]["image_url"]["detail"], "high");
    }

    #[test]
    fn translate_web_search_tool() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Text("Who is the president?".into()),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: None,
            stream: None,
            tools: Some(vec![ToolConfig::WebSearch {
                query: None,
                search_context_size: None,
            }]),
            store: None,
            previous_response_id: None,
            metadata: None,
        };

        let upstream = translate_request(&req).unwrap();
        let tools = upstream["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "web_search");
    }

    // ── Response Translation Tests ────────────────────────────────

    #[test]
    fn translate_openai_response() {
        let upstream = serde_json::json!({
            "id": "chatcmpl-abc123",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help you today?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            }
        });

        let resp = translate_response(&upstream, "gpt-4o");
        assert_eq!(resp.object, "response");
        assert!(resp.id.starts_with("resp_"));
        assert_eq!(resp.model, "gpt-4o");
        assert_eq!(resp.output.len(), 1);

        match &resp.output[0] {
            OutputItem::Message {
                content,
                role,
                status,
                ..
            } => {
                assert_eq!(role, "assistant");
                assert_eq!(status, "completed");
                assert_eq!(content.len(), 1);
                match &content[0] {
                    OutputContent::OutputText { text, .. } => {
                        assert_eq!(text, "Hello! How can I help you today?");
                    }
                    _ => panic!("Expected OutputText"),
                }
            }
        }

        let usage = resp.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.total_tokens, 30);
    }

    #[test]
    fn translate_response_no_usage() {
        let upstream = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hi" },
                "finish_reason": "stop"
            }]
        });

        let resp = translate_response(&upstream, "gpt-4o");
        assert!(resp.usage.is_none());
    }

    #[test]
    fn translate_response_length_finish() {
        let upstream = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Partial..." },
                "finish_reason": "length"
            }]
        });

        let resp = translate_response(&upstream, "gpt-4o");
        match &resp.output[0] {
            OutputItem::Message { status, .. } => {
                assert_eq!(
                    status, "completed",
                    "length finish is still 'completed' in Responses"
                );
            }
        }
    }

    // ── Streaming Event Tests ─────────────────────────────────────

    #[test]
    fn extract_upstream_delta_returns_content() {
        let chunk = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": { "content": "Hello" },
                "finish_reason": null
            }]
        });
        assert_eq!(extract_upstream_delta(&chunk), Some("Hello".to_string()));
    }

    #[test]
    fn extract_upstream_delta_empty_returns_none() {
        let chunk = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": { "content": "" },
                "finish_reason": null
            }]
        });
        assert_eq!(extract_upstream_delta(&chunk), None);
    }

    #[test]
    fn is_upstream_done_detects_done_signal() {
        let done = serde_json::json!("[DONE]");
        assert!(is_upstream_done(&done));
        let not_done = serde_json::json!({"choices": []});
        assert!(!is_upstream_done(&not_done));
    }

    #[test]
    fn translate_streaming_chunk_accumulates_text() {
        let chunk = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": { "content": "World" },
                "finish_reason": null
            }]
        });

        let mut text = String::new();
        let mut model_name = None;
        let mut usage_input = 0;
        let mut usage_output = 0;
        let result = translate_streaming_chunk(
            &chunk,
            &mut text,
            &mut model_name,
            &mut usage_input,
            &mut usage_output,
        );

        assert!(result.is_some());
        let (sse_str, delta) = result.unwrap();
        assert_eq!(delta, "World");
        assert_eq!(text, "World");
        assert!(sse_str.contains("response.output_text.delta"));
        assert!(sse_str.contains("World"));
        assert!(sse_str.contains("\"index\":0"));
    }

    #[test]
    fn sse_event_formatting() {
        let data = serde_json::json!({"type": "test", "value": 42});
        let formatted = sse_events::format_sse_event("test.event", &data);
        assert!(formatted.starts_with("event: test.event\n"));
        assert!(formatted.contains("data: "));
        assert!(formatted.ends_with("\n\n"));
    }

    // ── Capability Detection Tests ────────────────────────────────

    #[test]
    fn detect_capability_text_only() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Text("Hello".into()),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: None,
            stream: None,
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };
        let caps = detect_capability(&req);
        assert!(caps.contains(&crate::repositories::channel::ModelCapability::Chat));
        assert_eq!(caps.len(), 1);
    }

    #[test]
    fn detect_capability_with_image() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Messages(vec![InputMessage {
                role: "user".into(),
                content: Content::Parts(vec![ContentPart::InputImage {
                    image_url: "https://example.com/img.jpg".into(),
                    detail: None,
                }]),
            }]),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: None,
            stream: None,
            tools: None,
            store: None,
            previous_response_id: None,
            metadata: None,
        };
        let caps = detect_capability(&req);
        assert!(caps.contains(&crate::repositories::channel::ModelCapability::Chat));
        assert!(caps.contains(&crate::repositories::channel::ModelCapability::Vision));
    }

    #[test]
    fn detect_capability_with_web_search() {
        let req = ResponsesRequest {
            model: None,
            input: Input::Text("Search something".into()),
            instructions: None,
            developer_instructions: None,
            max_output_tokens: None,
            stream: None,
            tools: Some(vec![ToolConfig::WebSearch {
                query: None,
                search_context_size: None,
            }]),
            store: None,
            previous_response_id: None,
            metadata: None,
        };
        let caps = detect_capability(&req);
        assert!(caps.contains(&crate::repositories::channel::ModelCapability::Chat));
        assert!(caps.contains(&crate::repositories::channel::ModelCapability::WebSearch));
    }

    // ── Anthropic Translation Tests ────────────────────────────

    #[test]
    fn chat_completions_to_anthropic_basic() {
        let cc = serde_json::json!({
            "messages": [
                {"role": "user", "content": "Hello"}
            ],
            "max_tokens": 4096,
            "stream": true
        });
        let anthropic = chat_completions_to_anthropic(&cc);
        assert_eq!(anthropic["max_tokens"], 4096);
        assert_eq!(anthropic["stream"], true);
        assert!(
            anthropic.get("system").is_none(),
            "no system message should not set system field"
        );
        let msgs = anthropic["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "Hello");
    }

    #[test]
    fn chat_completions_to_anthropic_system_prompt() {
        let cc = serde_json::json!({
            "messages": [
                {"role": "system", "content": "You are a helpful assistant"},
                {"role": "user", "content": "Hi"}
            ]
        });
        let anthropic = chat_completions_to_anthropic(&cc);
        assert_eq!(anthropic["system"], "You are a helpful assistant");
        let msgs = anthropic["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn chat_completions_to_anthropic_developer_merged() {
        let cc = serde_json::json!({
            "messages": [
                {"role": "developer", "content": "Always respond in JSON"},
                {"role": "user", "content": "Hello"}
            ]
        });
        let anthropic = chat_completions_to_anthropic(&cc);
        // Developer role is merged into system field for Anthropic
        assert_eq!(anthropic["system"], "Always respond in JSON");
    }

    #[test]
    fn chat_completions_to_anthropic_stop_as_string() {
        let cc = serde_json::json!({
            "messages": [{"role": "user", "content": "Hi"}],
            "stop": "quit"
        });
        let anthropic = chat_completions_to_anthropic(&cc);
        // String stop should be converted to array
        let sequences = anthropic["stop_sequences"].as_array().unwrap();
        assert_eq!(sequences.len(), 1);
        assert_eq!(sequences[0], "quit");
    }

    #[test]
    fn chat_completions_to_anthropic_stop_as_array() {
        let cc = serde_json::json!({
            "messages": [{"role": "user", "content": "Hi"}],
            "stop": ["quit", "exit"]
        });
        let anthropic = chat_completions_to_anthropic(&cc);
        let sequences = anthropic["stop_sequences"].as_array().unwrap();
        assert_eq!(sequences.len(), 2);
    }

    #[test]
    fn chat_completions_to_anthropic_system_content_array() {
        let cc = serde_json::json!({
            "messages": [
                {
                    "role": "system",
                    "content": [
                        {"type": "text", "text": "Rule 1: be helpful"},
                        {"type": "text", "text": "Rule 2: be concise"}
                    ]
                },
                {"role": "user", "content": "OK"}
            ]
        });
        let anthropic = chat_completions_to_anthropic(&cc);
        // Array content should be extracted into a single system string
        let system = anthropic["system"].as_str().unwrap();
        assert!(system.contains("Rule 1"));
        assert!(system.contains("Rule 2"));
    }

    #[test]
    fn anthropic_response_to_chat_completions_basic() {
        let anthropic_resp = serde_json::json!({
            "id": "msg_abc123",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Hello! How can I help?"}
            ],
            "model": "claude-3-opus-20240229",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 25
            }
        });
        let cc = anthropic_response_to_chat_completions(&anthropic_resp);
        assert_eq!(cc["object"], "chat.completion");
        assert_eq!(cc["model"], "claude-3-opus-20240229");
        let choice = &cc["choices"][0];
        assert_eq!(choice["message"]["role"], "assistant");
        assert_eq!(choice["message"]["content"], "Hello! How can I help?");
        assert_eq!(
            choice["finish_reason"], "stop",
            "end_turn should map to stop"
        );
        assert_eq!(cc["usage"]["prompt_tokens"], 10);
        assert_eq!(cc["usage"]["completion_tokens"], 25);
        assert_eq!(cc["usage"]["total_tokens"], 35);
    }

    #[test]
    fn anthropic_response_no_usage() {
        let anthropic_resp = serde_json::json!({
            "id": "msg_xyz",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "OK"}],
            "model": "claude-3",
            "stop_reason": "end_turn"
        });
        let cc = anthropic_response_to_chat_completions(&anthropic_resp);
        assert!(
            cc.get("usage").is_some(),
            "usage should be present even if empty"
        );
        assert_eq!(cc["usage"]["prompt_tokens"], 0);
        assert_eq!(cc["usage"]["completion_tokens"], 0);
    }

    #[test]
    fn anthropic_response_max_tokens_finish() {
        let anthropic_resp = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Partial..."}],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 5, "output_tokens": 100}
        });
        let cc = anthropic_response_to_chat_completions(&anthropic_resp);
        assert_eq!(
            cc["choices"][0]["finish_reason"], "length",
            "max_tokens should map to length"
        );
    }

    #[test]
    fn extract_anthropic_streaming_delta_text() {
        let chunk = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "text_delta",
                "text": "Hello world"
            }
        });
        assert_eq!(
            extract_anthropic_streaming_delta(&chunk),
            Some("Hello world".to_string())
        );
    }

    #[test]
    fn extract_anthropic_streaming_delta_wrong_type() {
        let chunk = serde_json::json!({
            "type": "content_block_delta",
            "delta": {"type": "thinking_delta", "thinking": "..."}
        });
        assert_eq!(extract_anthropic_streaming_delta(&chunk), None);
    }

    #[test]
    fn extract_anthropic_usage_from_message_delta() {
        let chunk = serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 15, "output_tokens": 30}
        });
        let result = extract_anthropic_usage(&chunk);
        assert!(result.is_some());
        let (input, output) = result.unwrap();
        assert_eq!(input, 15);
        assert_eq!(output, 30);
    }

    #[test]
    fn extract_anthropic_usage_non_delta_returns_none() {
        let chunk = serde_json::json!({"type": "message_start"});
        assert!(extract_anthropic_usage(&chunk).is_none());
    }
}
