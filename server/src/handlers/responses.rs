//! Unified `/api/ai/response` handler for all AI Gateway capabilities.
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
    ResponsesRequest, detect_capability, is_valid_chat_completions_response, sse_events,
    translate_request, translate_response, translate_streaming_chunk,
};
use ains_runtime::{HttpError, RequestContext, Response};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
};
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

pub async fn ai_response(req: crate::ServerRequest) -> Result<Response, HttpError> {
    match ai_response_inner(req).await {
        Ok(response) => Ok(response),
        Err(error) => failed_response(error),
    }
}

async fn ai_response_inner(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = helpers::extract_handler_context(&req)?;

    // Tenant check
    if actor.role != "system" {
        require_active_tenant(&state, &actor.tenant_id).await?;
    }

    let mut body: Value = req.parse_json().await.map_err(HttpError::bad_request)?;
    validate_requested_model(&body)?;
    let requested_capability = parse_requested_capability(&body)?;

    if let Some(
        capability @ (crate::repositories::channel::ModelCapability::Embedding
        | crate::repositories::channel::ModelCapability::Stt
        | crate::repositories::channel::ModelCapability::Tts),
    ) = requested_capability
    {
        return handle_direct_capability(&state, &actor, capability, body).await;
    }

    let original_body = body.clone();
    // `capability` is an AINS routing field, not an upstream Responses field.
    if let Some(object) = body.as_object_mut() {
        object.remove("capability");
    }
    let responses_req: ResponsesRequest = serde_json::from_value(body)
        .map_err(|e| HttpError::bad_request(format!("Invalid Responses API request: {}", e)))?;
    validate_generation_options(&responses_req)?;

    // Detect required capabilities from the request
    let capabilities = detect_capability(&responses_req);
    validate_capability_combination(&capabilities)?;

    let primary_capability = requested_capability.clone().unwrap_or_else(|| {
        // Prefer the most specific detected capability. Chat is always present,
        // so taking the first element would route images to chat-only channels.
        capabilities
            .iter()
            .find(|capability| **capability != crate::repositories::channel::ModelCapability::Chat)
            .cloned()
            .unwrap_or(crate::repositories::channel::ModelCapability::Chat)
    });

    if primary_capability == crate::repositories::channel::ModelCapability::Stt {
        return handle_direct_capability(&state, &actor, primary_capability, original_body).await;
    }

    let required_capabilities = required_capabilities(&capabilities, requested_capability.as_ref());

    // Translate the request to upstream Chat Completions format
    let upstream_body = translate_request(&responses_req)
        .map_err(|e| HttpError::bad_request(format!("Request translation error: {}", e)))?;

    // Check streaming mode
    let is_streaming = responses_req.stream.unwrap_or(false);

    if is_streaming {
        handle_streaming(
            &state,
            &actor,
            primary_capability,
            &required_capabilities,
            upstream_body,
        )
        .await
    } else {
        handle_non_streaming(
            &state,
            &actor,
            primary_capability,
            &required_capabilities,
            upstream_body,
            &responses_req,
        )
        .await
    }
}

fn validate_requested_model(body: &Value) -> Result<(), HttpError> {
    let Some(model) = body.get("model") else {
        return Ok(());
    };
    if model.as_str().is_some_and(|model| !model.trim().is_empty()) {
        return Ok(());
    }
    Err(HttpError::bad_request(
        "model must be a non-empty string when provided",
    ))
}

fn validate_generation_options(request: &ResponsesRequest) -> Result<(), HttpError> {
    if request.max_output_tokens == Some(0) {
        return Err(HttpError::bad_request(
            "max_output_tokens must be greater than zero",
        ));
    }
    if let Some(temperature) = request.temperature
        && !(0.0..=1.0).contains(&temperature)
    {
        return Err(HttpError::bad_request(
            "temperature must be between 0 and 1",
        ));
    }
    if request
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
    {
        return Err(HttpError::bad_request(
            "tools are not supported by this AI response endpoint",
        ));
    }
    if request.store == Some(true) {
        return Err(HttpError::bad_request(
            "store=true is not supported by this stateless AI response endpoint",
        ));
    }
    if request.previous_response_id.is_some() {
        return Err(HttpError::bad_request(
            "previous_response_id is not supported by this stateless AI response endpoint",
        ));
    }
    Ok(())
}

fn failed_response(error: HttpError) -> Result<Response, HttpError> {
    let status = error.status;
    let body = ains_runtime::ai_response_error_body(error.error_type, &error.message);
    let mut response = Response::json(&body)?;
    response.set_status(status);
    Ok(response)
}

fn validate_capability_combination(
    capabilities: &[crate::repositories::channel::ModelCapability],
) -> Result<(), HttpError> {
    use crate::repositories::channel::ModelCapability;

    if capabilities.contains(&ModelCapability::Stt)
        && capabilities.iter().any(|capability| {
            matches!(
                capability,
                ModelCapability::Vision | ModelCapability::WebSearch
            )
        })
    {
        return Err(HttpError::bad_request(
            "STT cannot be combined with vision or web search in one request",
        ));
    }
    Ok(())
}

fn required_capabilities(
    detected: &[crate::repositories::channel::ModelCapability],
    requested: Option<&crate::repositories::channel::ModelCapability>,
) -> Vec<crate::repositories::channel::ModelCapability> {
    let mut required = detected.to_vec();
    if let Some(requested) = requested
        && !required.contains(requested)
    {
        required.push(requested.clone());
    }
    required
}

fn parse_requested_capability(
    body: &Value,
) -> Result<Option<crate::repositories::channel::ModelCapability>, HttpError> {
    use crate::repositories::channel::ModelCapability;

    let Some(raw) = body.get("capability") else {
        return Ok(None);
    };
    let raw = raw
        .as_str()
        .ok_or_else(|| HttpError::bad_request("capability must be a string"))?;
    let capability = match raw {
        "chat" => ModelCapability::Chat,
        "vision" => ModelCapability::Vision,
        "embedding" | "embed" => ModelCapability::Embedding,
        "stt" => ModelCapability::Stt,
        "tts" => ModelCapability::Tts,
        "web_search" | "websearch" => ModelCapability::WebSearch,
        _ => return Err(HttpError::bad_request("Unsupported AI capability")),
    };
    Ok(Some(capability))
}

async fn handle_direct_capability(
    state: &crate::AppState,
    actor: &ains_runtime::AuthUser,
    capability: crate::repositories::channel::ModelCapability,
    mut body: Value,
) -> Result<Response, HttpError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| HttpError::bad_request("AI response request must be a JSON object"))?;
    object.remove("capability");
    normalize_direct_common_fields(object)?;

    // STT uses the unified input_audio content part externally, while the
    // existing gateway dispatcher consumes a flat base64 `file` field.
    if capability == crate::repositories::channel::ModelCapability::Stt {
        normalize_stt_body(object)?;
    }
    // TTS keeps speech configuration under `audio` in the unified contract,
    // while the OpenAI speech endpoint expects those fields at the top level.
    if capability == crate::repositories::channel::ModelCapability::Tts {
        normalize_tts_body(object)?;
    }

    validate_direct_fields(&capability, object)?;
    validate_direct_input(&capability, object)?;

    let mut result = service(state)
        .proxy(
            &actor.tenant_id,
            &actor.user_id,
            capability.clone(),
            std::slice::from_ref(&capability),
            body,
        )
        .await
        .map_err(error)?;
    normalize_and_validate_direct_result(&capability, &mut result)?;
    Response::json(&unified_direct_response(&capability, &result))
}

fn normalize_direct_common_fields(
    object: &mut serde_json::Map<String, Value>,
) -> Result<(), HttpError> {
    match object.remove("stream") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            return Err(HttpError::bad_request(
                "Streaming is only supported for chat and vision",
            ));
        }
        Some(_) => return Err(HttpError::bad_request("stream must be a boolean")),
    }

    match object.remove("store") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            return Err(HttpError::bad_request(
                "store=true is not supported by this stateless AI response endpoint",
            ));
        }
        Some(_) => return Err(HttpError::bad_request("store must be a boolean")),
    }

    match object.remove("tools") {
        None | Some(Value::Null) => {}
        Some(Value::Array(tools)) if tools.is_empty() => {}
        Some(Value::Array(_)) => {
            return Err(HttpError::bad_request(
                "tools are not supported by this AI response endpoint",
            ));
        }
        Some(_) => return Err(HttpError::bad_request("tools must be an array")),
    }

    match object.remove("previous_response_id") {
        None | Some(Value::Null) => {}
        Some(_) => {
            return Err(HttpError::bad_request(
                "previous_response_id is not supported by this stateless AI response endpoint",
            ));
        }
    }

    // Metadata belongs to the AINS envelope and is not an upstream capability
    // parameter. Validate the public shape before removing it; it can be added
    // to server-side tracing independently.
    match object.remove("metadata") {
        None | Some(Value::Null) => {}
        Some(Value::Object(metadata)) if metadata.values().all(Value::is_string) => {}
        Some(_) => {
            return Err(HttpError::bad_request(
                "metadata must be an object with string values",
            ));
        }
    }

    Ok(())
}

fn validate_direct_input(
    capability: &crate::repositories::channel::ModelCapability,
    object: &serde_json::Map<String, Value>,
) -> Result<(), HttpError> {
    use crate::repositories::channel::ModelCapability;
    match capability {
        ModelCapability::Embedding => {
            let valid = match object.get("input") {
                Some(Value::String(input)) => !input.trim().is_empty(),
                Some(Value::Array(values)) => {
                    !values.is_empty()
                        && values.iter().all(|value| {
                            value.as_str().is_some_and(|input| !input.trim().is_empty())
                        })
                }
                _ => false,
            };
            if !valid {
                return Err(HttpError::bad_request(
                    "Embedding input must be a string or non-empty string array",
                ));
            }
        }
        ModelCapability::Tts
            if object
                .get("input")
                .and_then(Value::as_str)
                .is_none_or(|input| input.trim().is_empty()) =>
        {
            return Err(HttpError::bad_request(
                "TTS input must be a non-empty string",
            ));
        }
        _ => {}
    }
    Ok(())
}

fn validate_direct_fields(
    capability: &crate::repositories::channel::ModelCapability,
    object: &serde_json::Map<String, Value>,
) -> Result<(), HttpError> {
    use crate::repositories::channel::ModelCapability;

    let allowed: &[&str] = match capability {
        ModelCapability::Embedding => &["model", "input", "encoding_format", "dimensions", "user"],
        ModelCapability::Stt => &[
            "model",
            "file",
            "filename",
            "language",
            "prompt",
            "response_format",
            "temperature",
        ],
        ModelCapability::Tts => &[
            "model",
            "input",
            "voice",
            "response_format",
            "speed",
            "instructions",
            "stream_format",
        ],
        _ => return Ok(()),
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(HttpError::bad_request(format!(
            "Unsupported {} field: {field}",
            capability.as_str()
        )));
    }

    match capability {
        ModelCapability::Embedding => {
            if let Some(encoding) = object.get("encoding_format")
                && !matches!(encoding.as_str(), Some("float" | "base64"))
            {
                return Err(HttpError::bad_request(
                    "Embedding encoding_format must be 'float' or 'base64'",
                ));
            }
            if let Some(dimensions) = object.get("dimensions")
                && dimensions.as_u64().is_none_or(|dimensions| dimensions == 0)
            {
                return Err(HttpError::bad_request(
                    "Embedding dimensions must be a positive integer",
                ));
            }
            if let Some(user) = object.get("user")
                && user.as_str().is_none_or(|user| user.trim().is_empty())
            {
                return Err(HttpError::bad_request(
                    "Embedding user must be a non-empty string",
                ));
            }
        }
        ModelCapability::Stt => validate_stt_fields(object)?,
        ModelCapability::Tts => {}
        _ => {}
    }
    Ok(())
}

fn validate_stt_fields(object: &serde_json::Map<String, Value>) -> Result<(), HttpError> {
    let encoded_file = object
        .get("file")
        .and_then(Value::as_str)
        .filter(|file| !file.trim().is_empty())
        .ok_or_else(|| {
            HttpError::bad_request("STT input_audio.data must be a non-empty base64 string")
        })?;
    let audio_bytes = URL_SAFE_NO_PAD
        .decode(encoded_file)
        .or_else(|_| BASE64_STANDARD.decode(encoded_file))
        .map_err(|_| HttpError::bad_request("STT input_audio.data must be valid base64"))?;
    if audio_bytes.is_empty() {
        return Err(HttpError::bad_request(
            "STT input_audio.data must decode to non-empty audio data",
        ));
    }
    let extension = object
        .get("filename")
        .and_then(Value::as_str)
        .and_then(|filename| filename.rsplit_once('.').map(|(_, extension)| extension))
        .map(str::to_ascii_lowercase);
    if !extension
        .as_deref()
        .is_some_and(is_supported_stt_audio_format)
    {
        return Err(HttpError::bad_request("Unsupported STT audio format"));
    }
    if object
        .get("filename")
        .and_then(Value::as_str)
        .is_none_or(|filename| filename.trim().is_empty())
    {
        return Err(HttpError::bad_request(
            "STT filename must be a non-empty string",
        ));
    }
    for field in ["language", "prompt"] {
        if let Some(value) = object.get(field)
            && value.as_str().is_none_or(|value| value.trim().is_empty())
        {
            return Err(HttpError::bad_request(format!(
                "STT {field} must be a non-empty string"
            )));
        }
    }
    if let Some(format) = object.get("response_format") {
        const FORMATS: &[&str] = &["json", "text", "srt", "verbose_json", "vtt"];
        if !format
            .as_str()
            .is_some_and(|format| FORMATS.contains(&format))
        {
            return Err(HttpError::bad_request("Unsupported STT response_format"));
        }
    }
    if let Some(temperature) = object.get("temperature")
        && !temperature
            .as_f64()
            .is_some_and(|temperature| (0.0..=1.0).contains(&temperature))
    {
        return Err(HttpError::bad_request(
            "STT temperature must be between 0 and 1",
        ));
    }
    Ok(())
}

fn unified_direct_response(
    capability: &crate::repositories::channel::ModelCapability,
    result: &Value,
) -> Value {
    let output = match capability {
        crate::repositories::channel::ModelCapability::Embedding => embedding_output(result),
        crate::repositories::channel::ModelCapability::Stt => serde_json::json!([{
            "type": "transcription",
            "text": result.get("text").cloned().unwrap_or(Value::Null)
        }]),
        crate::repositories::channel::ModelCapability::Tts => serde_json::json!([{
            "type": "audio",
            "data": result.get("audio").cloned().unwrap_or(Value::Null),
            "content_type": result.get("content_type").cloned().unwrap_or(Value::Null)
        }]),
        _ => result.clone(),
    };
    serde_json::json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "model": result.get("model").cloned().unwrap_or(Value::Null),
        "capability": capability.as_str(),
        "status": "completed",
        "incomplete_details": Value::Null,
        "output": output,
        "usage": unified_usage(result),
        "error": Value::Null
    })
}

fn normalize_and_validate_direct_result(
    capability: &crate::repositories::channel::ModelCapability,
    result: &mut Value,
) -> Result<(), HttpError> {
    use crate::repositories::channel::ModelCapability;

    let invalid = || {
        tracing::warn!(
            capability = capability.as_str(),
            "AI provider returned an invalid response"
        );
        HttpError::service_unavailable("AI provider returned an invalid response")
    };

    match capability {
        ModelCapability::Embedding => {
            let items = result
                .get_mut("data")
                .and_then(Value::as_array_mut)
                .filter(|items| !items.is_empty())
                .ok_or_else(invalid)?;
            for item in items {
                let object = item.as_object_mut().ok_or_else(invalid)?;
                let dimensions = {
                    let embedding = object.get_mut("embedding").ok_or_else(invalid)?;
                    if let Some(encoded) = embedding.as_str() {
                        let bytes = BASE64_STANDARD.decode(encoded).map_err(|_| invalid())?;
                        if bytes.is_empty() || bytes.len() % std::mem::size_of::<f32>() != 0 {
                            return Err(invalid());
                        }
                        let values = bytes
                            .chunks_exact(4)
                            .map(|chunk| {
                                f32::from_le_bytes(chunk.try_into().expect("four-byte chunk"))
                            })
                            .collect::<Vec<_>>();
                        if values.iter().any(|value| !value.is_finite()) {
                            return Err(invalid());
                        }
                        *embedding = serde_json::json!(values);
                    }
                    embedding
                        .as_array()
                        .filter(|values| !values.is_empty() && values.iter().all(Value::is_number))
                        .map(Vec::len)
                        .ok_or_else(invalid)?
                };
                object.insert("dimensions".into(), serde_json::json!(dimensions));
            }
        }
        ModelCapability::Stt if result.get("text").and_then(Value::as_str).is_none() => {
            return Err(invalid());
        }
        ModelCapability::Tts
            if result
                .get("audio")
                .and_then(Value::as_str)
                .is_none_or(|audio| audio.is_empty())
                || result
                    .get("content_type")
                    .and_then(Value::as_str)
                    .is_none_or(|content_type| content_type.is_empty()) =>
        {
            return Err(invalid());
        }
        _ => {}
    }
    Ok(())
}

fn unified_usage(result: &Value) -> Value {
    let Some(usage) = result.get("usage").and_then(Value::as_object) else {
        return Value::Null;
    };
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    serde_json::json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens
    })
}

fn embedding_output(result: &Value) -> Value {
    let Some(items) = result.get("data").and_then(Value::as_array) else {
        return result
            .get("data")
            .cloned()
            .unwrap_or_else(|| result.clone());
    };

    Value::Array(
        items
            .iter()
            .map(|item| {
                let mut item = item.clone();
                if let Some(object) = item.as_object_mut()
                    && let Some(dimensions) = object
                        .get("embedding")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                {
                    object.insert("dimensions".into(), serde_json::json!(dimensions));
                }
                item
            })
            .collect(),
    )
}

fn normalize_tts_body(object: &mut serde_json::Map<String, Value>) -> Result<(), HttpError> {
    let audio = match object.remove("audio") {
        None => serde_json::Map::new(),
        Some(Value::Object(audio)) => audio,
        Some(_) => return Err(HttpError::bad_request("TTS audio must be an object")),
    };

    const AUDIO_FIELDS: [(&str, &str); 5] = [
        ("voice", "voice"),
        ("format", "response_format"),
        ("speed", "speed"),
        ("instructions", "instructions"),
        ("stream_format", "stream_format"),
    ];
    for key in audio.keys() {
        if !AUDIO_FIELDS.iter().any(|(source, _)| source == key) {
            return Err(HttpError::bad_request(format!(
                "Unsupported TTS audio field: {key}"
            )));
        }
    }
    for (source, target) in AUDIO_FIELDS {
        let Some(value) = audio.get(source) else {
            continue;
        };
        if let Some(existing) = object.get(target) {
            if existing != value {
                return Err(HttpError::bad_request(format!(
                    "TTS audio.{source} conflicts with top-level {target}"
                )));
            }
        } else {
            object.insert(target.into(), value.clone());
        }
    }

    let voice = object
        .get("voice")
        .and_then(Value::as_str)
        .filter(|voice| !voice.trim().is_empty())
        .ok_or_else(|| HttpError::bad_request("TTS voice must be a non-empty string"))?;
    // Store the trimmed value so whitespace-only padding is not forwarded.
    object.insert("voice".into(), Value::String(voice.trim().to_string()));

    if let Some(format) = object.get("response_format") {
        let format = format
            .as_str()
            .filter(|format| !format.trim().is_empty())
            .ok_or_else(|| {
                HttpError::bad_request("TTS response_format must be a non-empty string")
            })?;
        // Validate the requested format up front (before channel selection and
        // quota consumption) so an unsupported format like "avi" fails fast
        // with 400 instead of after a channel has been picked.
        if !is_supported_tts_format(&format.to_ascii_lowercase()) {
            return Err(HttpError::bad_request(format!(
                "Unsupported TTS response_format: {format}"
            )));
        }
    }
    if let Some(speed) = object.get("speed") {
        let valid = speed
            .as_f64()
            .is_some_and(|speed| (0.25..=4.0).contains(&speed));
        if !valid {
            return Err(HttpError::bad_request(
                "TTS speed must be a number between 0.25 and 4.0",
            ));
        }
    }
    if let Some(instructions) = object.get("instructions")
        && !instructions.is_string()
    {
        return Err(HttpError::bad_request("TTS instructions must be a string"));
    }
    if let Some(stream_format) = object.get("stream_format")
        && stream_format.as_str() != Some("audio")
    {
        return Err(HttpError::bad_request(
            "TTS stream_format must be 'audio' for non-streaming responses",
        ));
    }

    Ok(())
}

fn normalize_stt_body(object: &mut serde_json::Map<String, Value>) -> Result<(), HttpError> {
    if object.get("file").and_then(Value::as_str).is_some() {
        return Ok(());
    }
    // Reject mixed media up front. STT consumes exactly one audio input; an
    // accompanying image would be silently dropped when `input` is removed
    // below, so surface a 400 instead of transcribing and losing the image.
    if has_input_image(object) {
        return Err(HttpError::bad_request(
            "STT input cannot be combined with image content",
        ));
    }
    let audio = find_audio_input(object)?
        .ok_or_else(|| HttpError::bad_request("STT input must contain input_audio data"))?;
    let data = audio
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError::bad_request("STT input_audio.data is required"))?
        .to_string();
    let format = match audio.get("format") {
        None => "wav".to_string(),
        Some(Value::String(format))
            if is_supported_stt_audio_format(&format.to_ascii_lowercase()) =>
        {
            format.to_ascii_lowercase()
        }
        Some(_) => return Err(HttpError::bad_request("Unsupported STT audio format")),
    };
    let filename = format!("audio.{format}");
    object.insert("file".into(), Value::String(data));
    object.insert("filename".into(), Value::String(filename));
    object.remove("input");
    object.remove("input_audio");
    Ok(())
}

fn is_supported_stt_audio_format(format: &str) -> bool {
    matches!(
        format,
        "flac" | "mp3" | "mp4" | "mpeg" | "mpga" | "m4a" | "ogg" | "wav" | "webm"
    )
}

/// TTS output formats accepted by the upstream speech endpoint. Kept in sync
/// with the gateway's `tts_format_content_type`.
fn is_supported_tts_format(format: &str) -> bool {
    matches!(format, "mp3" | "opus" | "aac" | "flac" | "wav" | "pcm")
}

/// Whether the request carries any `input_image` content part in its messages.
fn has_input_image(object: &serde_json::Map<String, Value>) -> bool {
    object
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
}

fn find_audio_input(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<&serde_json::Map<String, Value>>, HttpError> {
    let mut matches = object
        .get("input")
        .and_then(Value::as_object)
        .into_iter()
        .chain(object.get("input_audio").and_then(Value::as_object))
        .chain(
            object
                .get("input")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|message| {
                    message
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("input_audio"))
                .filter_map(Value::as_object),
        );
    let Some(first) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(HttpError::bad_request(
            "STT accepts exactly one input_audio part per request",
        ));
    }
    Ok(Some(first))
}

// ── Non-streaming handler ────────────────────────────────────────

async fn handle_non_streaming(
    state: &crate::AppState,
    actor: &ains_runtime::AuthUser,
    capability: crate::repositories::channel::ModelCapability,
    required_capabilities: &[crate::repositories::channel::ModelCapability],
    upstream_body: Value,
    _req: &ResponsesRequest,
) -> Result<Response, HttpError> {
    // Proxy via existing non-streaming method
    let result = service(state)
        .proxy(
            &actor.tenant_id,
            &actor.user_id,
            capability.clone(),
            required_capabilities,
            upstream_body,
        )
        .await
        .map_err(error)?;

    validate_chat_result(&result)?;

    // Translate upstream response to Responses API format
    let model = result
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");

    let mut responses_response = translate_response(&result, model);
    responses_response.capability = capability.as_str().to_string();

    Response::json(&responses_response)
}

fn validate_chat_result(result: &Value) -> Result<(), HttpError> {
    if is_valid_chat_completions_response(result) {
        Ok(())
    } else {
        tracing::warn!("AI provider returned an invalid chat response");
        Err(HttpError::service_unavailable(
            "AI provider returned an invalid response",
        ))
    }
}

// ── Streaming handler ────────────────────────────────────────────

#[derive(Debug, Default)]
struct StreamTerminationState {
    saw_done: bool,
    finish_reason: Option<String>,
}

impl StreamTerminationState {
    fn is_completed(&self) -> bool {
        // The gateway forwards the terminal `[DONE]` marker only on a genuine
        // upstream completion (OpenAI `[DONE]` or Anthropic `message_stop`); any
        // mid-stream failure yields an error event and closes the channel
        // *without* `[DONE]`. So once `[DONE]` is observed the response is
        // complete unless the provider's finish_reason explicitly signals
        // truncation (`length`) or filtering (`content_filter`). A missing or
        // vendor-specific finish_reason (e.g. Anthropic without a mapped
        // stop_reason, or `tool_calls`) must therefore still be treated as a
        // successful completion rather than a spurious `response.failed`.
        self.saw_done && self.incomplete_reason().is_none()
    }

    fn incomplete_reason(&self) -> Option<&'static str> {
        if self.saw_done && self.finish_reason.as_deref() == Some("length") {
            Some("max_output_tokens")
        } else if self.saw_done && self.finish_reason.as_deref() == Some("content_filter") {
            Some("content_filter")
        } else {
            None
        }
    }

    fn is_incomplete(&self) -> bool {
        self.incomplete_reason().is_some()
    }

    fn response_status(&self) -> &'static str {
        if self.is_completed() {
            "completed"
        } else if self.is_incomplete() {
            "incomplete"
        } else {
            "failed"
        }
    }

    fn terminal_event_name(&self) -> &'static str {
        if self.is_completed() {
            "response.completed"
        } else if self.is_incomplete() {
            "response.incomplete"
        } else {
            "response.failed"
        }
    }

    fn observe(&mut self, event_str: &str) {
        let Some(data) = upstream_event_data(event_str) else {
            return;
        };
        if data == "[DONE]" {
            self.saw_done = true;
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        if let Some(reason) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| {
                choices
                    .iter()
                    .find_map(|choice| choice.get("finish_reason").and_then(Value::as_str))
            })
        {
            self.finish_reason = Some(reason.to_string());
        }
    }
}

fn failed_stream_response_error() -> Value {
    serde_json::json!({
        "code": "server_error",
        "message": "AI provider stream failed"
    })
}

fn take_next_sse_event(buffer: &mut Vec<u8>) -> Option<Result<String, std::string::FromUtf8Error>> {
    let (position, delimiter_len) = find_sse_delimiter(buffer)?;
    let remaining = buffer.split_off(position + delimiter_len);
    let mut event = std::mem::replace(buffer, remaining);
    event.truncate(position);
    Some(String::from_utf8(event))
}

fn find_sse_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    for position in 0..buffer.len() {
        if buffer[position..].starts_with(b"\r\n\r\n") {
            return Some((position, 4));
        }
        if buffer[position..].starts_with(b"\n\n") || buffer[position..].starts_with(b"\r\r") {
            return Some((position, 2));
        }
    }
    None
}

/// Decode the `data:` payload of an SSE event block.
///
/// Per the SSE spec (and matching the gateway's `sse_data_payload`), an event
/// may carry the payload across several `data:` lines which are joined with a
/// newline; a single leading space after each colon is stripped. Comment lines
/// (`:` prefix) and other fields are ignored. Returns `None` when the block
/// carries no `data:` line at all.
fn upstream_event_data(event_str: &str) -> Option<String> {
    let mut found = false;
    let mut payload = String::new();
    for line in event_str.lines() {
        let Some(value) = line.strip_prefix("data:") else {
            continue;
        };
        if found {
            payload.push('\n');
        }
        payload.push_str(value.strip_prefix(' ').unwrap_or(value));
        found = true;
    }
    found.then(|| payload.trim().to_string())
}

fn upstream_event_type(event_str: &str) -> Option<&str> {
    event_str
        .lines()
        .find_map(|line| line.strip_prefix("event:").map(str::trim))
}

async fn send_stream_error(
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, String>>,
    sequence_number: &mut u64,
    code: &str,
    message: &str,
) {
    let event = sse_events::error_event(code, message, *sequence_number);
    *sequence_number = sequence_number.saturating_add(1);
    let event = sse_events::format_sse_event("error", &event);
    // The terminal `error` event is the only signal that distinguishes a failed
    // stream from a clean completion. A momentarily-full client queue must not
    // cause it to be dropped (a bare `try_send` would surface as an ordinary
    // EOF and be mistaken for success). Wait briefly for capacity, but bound the
    // wait so a client that is truly gone cannot pin the upstream connection or
    // delay metering indefinitely.
    let send = tx.send(Ok(Bytes::from(event)));
    let _ = tokio::time::timeout(STREAM_ERROR_SEND_TIMEOUT, send).await;
}

/// Upper bound on how long the terminal `error` event waits for a busy client
/// queue to drain before cleanup proceeds without it.
const STREAM_ERROR_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn record_stream_usage(
    metering: &MeteringService,
    user_id: &str,
    tenant_id: &str,
    channel_id: uuid::Uuid,
    model: &str,
    capability: &str,
    response: &Value,
) {
    let uid: i64 = user_id.parse().unwrap_or_else(|error| {
        tracing::error!(
            error = %error,
            raw_user_id = %user_id,
            tenant_id,
            channel_id = %channel_id,
            "Failed to parse user_id as i64 — metering will use 0"
        );
        0
    });
    let record = metering.record_usage(uid, tenant_id, channel_id, model, capability, response);
    match tokio::time::timeout(std::time::Duration::from_secs(5), record).await {
        Err(_) => tracing::warn!(channel_id = %channel_id, "Streaming usage metering timed out"),
        Ok(Err(error)) => tracing::warn!(
            channel_id = %channel_id,
            error = %error,
            "Failed to persist streaming usage"
        ),
        Ok(Ok(_)) => {}
    }
}

fn partial_stream_metering_payload(
    model: &str,
    capability: &str,
    usage_input: u64,
    usage_output: u64,
    input_estimate: u64,
    accumulated_text: &str,
) -> Value {
    // An interrupted stream may carry no prompt-usage frame; fall back to the
    // request's character-based input estimate so the input side is still
    // billed rather than recorded as zero.
    let usage_input = if usage_input == 0 {
        input_estimate
    } else {
        usage_input
    };
    let usage_output = if usage_output == 0 && !accumulated_text.is_empty() {
        (accumulated_text.len() as u64 / 4).max(1)
    } else {
        usage_output
    };
    serde_json::json!({
        "model": model,
        "capability": capability,
        "status": "failed",
        "usage": {
            "input_tokens": usage_input,
            "output_tokens": usage_output,
            "total_tokens": usage_input.saturating_add(usage_output)
        }
    })
}

async fn handle_streaming(
    state: &crate::AppState,
    actor: &ains_runtime::AuthUser,
    capability: crate::repositories::channel::ModelCapability,
    required_capabilities: &[crate::repositories::channel::ModelCapability],
    upstream_body: Value,
) -> Result<Response, HttpError> {
    // Start streaming proxy — this creates a background task that reads
    // streaming chunks from the upstream and sends them through a channel.
    // The returned channel_id identifies which channel was selected for
    // downstream token metering recording.
    let capability_name = capability.as_str().to_string();
    let (mut upstream_rx, channel_id, selected_model, input_token_estimate) = service(state)
        .proxy_stream(
            &actor.tenant_id,
            &actor.user_id,
            capability,
            required_capabilities,
            upstream_body,
        )
        .await
        .map_err(error)?;

    // Prepare metering dependencies before entering the spawned task.
    // The spawned closure cannot borrow `state` or `actor`.
    let metering = MeteringService::new(state.db.clone());
    let meter_tenant_id = actor.tenant_id.clone();
    let meter_user_id = actor.user_id.clone();

    // Keep the downstream queue bounded so a slow client applies backpressure
    // through the translator to the upstream HTTP stream.
    const SSE_CHANNEL_CAPACITY: usize = 32;
    let (sse_tx, sse_rx) = tokio::sync::mpsc::channel(SSE_CHANNEL_CAPACITY);

    // Spawn a task that reads from the upstream stream, translates events,
    // and sends them through the SSE channel.
    // A keepalive interval is also included to prevent proxy/load-balancer
    // timeouts on long-running SSE connections with infrequent data.
    //
    // MAX_SSE_BUFFER: safety limit to prevent unbounded memory growth when
    // the upstream sends data without SSE event terminators (\n\n).
    const MAX_SSE_BUFFER: usize = 1_048_576; // 1 MB
    tokio::spawn(async move {
        let created_at = chrono::Utc::now().timestamp();
        let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let mut sequence_number = 0u64;
        let mut text_accumulator = String::new();
        let mut refusal_accumulator = String::new();
        let mut usage_input: u64 = 0;
        let mut usage_output: u64 = 0;
        let mut model_name: Option<String> = None;
        let mut consecutive_parse_errors: u32 = 0;
        let mut termination = StreamTerminationState::default();

        // Buffer for accumulating partial SSE events across chunks
        let mut buffer = Vec::new();
        let mut keepalive_interval = tokio::time::interval(std::time::Duration::from_secs(15));
        // The first tick completes immediately; consume it so the first
        // keepalive fires after 15s instead of almost immediately.
        keepalive_interval.tick().await;

        // Every early exit still records any usage observed before the stream
        // failed or the client disconnected. Dropping upstream_rx first also
        // cancels the provider producer instead of retaining it during DB I/O.
        macro_rules! meter_and_return {
            () => {{
                drop(upstream_rx);
                drop(sse_tx);
                let final_model = model_name.as_deref().unwrap_or(&selected_model);
                let payload = partial_stream_metering_payload(
                    final_model,
                    &capability_name,
                    usage_input,
                    usage_output,
                    input_token_estimate,
                    &text_accumulator,
                );
                record_stream_usage(
                    &metering,
                    &meter_user_id,
                    &meter_tenant_id,
                    channel_id,
                    final_model,
                    &capability_name,
                    &payload,
                )
                .await;
                return;
            }};
        }

        let created_response = serde_json::json!({
            "id": response_id,
            "object": "response",
            "created_at": created_at,
            "model": selected_model,
            "capability": capability_name,
            "status": "in_progress",
            "output": [],
            "usage": Value::Null,
            "incomplete_details": Value::Null,
            "error": Value::Null
        });
        let created_event = serde_json::json!({
            "type": "response.created",
            "response": created_response,
            "sequence_number": sequence_number
        });
        sequence_number += 1;
        if sse_tx
            .send(Ok(Bytes::from(sse_events::format_sse_event(
                "response.created",
                &created_event,
            ))))
            .await
            .is_err()
        {
            meter_and_return!();
        }

        let initial_item = serde_json::json!({
            "id": msg_id,
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "content": []
        });
        let item_added = serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": initial_item,
            "sequence_number": sequence_number
        });
        sequence_number += 1;
        if sse_tx
            .send(Ok(Bytes::from(sse_events::format_sse_event(
                "response.output_item.added",
                &item_added,
            ))))
            .await
            .is_err()
        {
            meter_and_return!();
        }

        let initial_part = serde_json::json!({
            "type": "output_text",
            "text": "",
            "annotations": []
        });
        let part_added = serde_json::json!({
            "type": "response.content_part.added",
            "item_id": msg_id,
            "output_index": 0,
            "content_index": 0,
            "part": initial_part,
            "sequence_number": sequence_number
        });
        sequence_number += 1;
        if sse_tx
            .send(Ok(Bytes::from(sse_events::format_sse_event(
                "response.content_part.added",
                &part_added,
            ))))
            .await
            .is_err()
        {
            meter_and_return!();
        }

        'upstream: loop {
            tokio::select! {
                _ = sse_tx.closed() => {
                    meter_and_return!();
                }
                chunk_result = upstream_rx.recv() => {
                    let chunk = match chunk_result {
                        Some(c) => c,
                        None => break, // Stream ended
                    };
                    match chunk {
                        Ok(bytes) => {
                            // Buffer bytes until a complete SSE event is available. UTF-8
                            // code points may be split across arbitrary network chunks.
                            if buffer.len().saturating_add(bytes.len()) > MAX_SSE_BUFFER {
                                tracing::error!(
                                    buffer_bytes = buffer.len().saturating_add(bytes.len()),
                                    max = MAX_SSE_BUFFER,
                                    "SSE buffer overflow — terminating stream"
                                );
                                send_stream_error(
                                    &sse_tx,
                                    &mut sequence_number,
                                    "stream_buffer_overflow",
                                    "AI provider stream failed",
                                )
                                .await;
                                meter_and_return!();
                            }
                            buffer.extend_from_slice(&bytes);
                            while let Some(event) = take_next_sse_event(&mut buffer) {
                                let event_str = match event {
                                    Ok(event) => event,
                                    Err(error) => {
                                        tracing::warn!(
                                            error = %error,
                                            "Upstream SSE event is not valid UTF-8"
                                        );
                                        consecutive_parse_errors += 1;
                                        if consecutive_parse_errors >= MAX_CONSECUTIVE_PARSE_ERRORS {
                                            send_stream_error(
                                                &sse_tx,
                                                &mut sequence_number,
                                                "invalid_stream_data",
                                                "AI provider stream failed",
                                            )
                                            .await;
                                            meter_and_return!();
                                        }
                                        continue;
                                    }
                                };
                                termination.observe(&event_str);
                                match process_upstream_event(
                                    &event_str,
                                    &mut UpstreamEventContext {
                                        text_accumulator: &mut text_accumulator,
                                        refusal_accumulator: &mut refusal_accumulator,
                                        model_name: &mut model_name,
                                        usage_input: &mut usage_input,
                                        usage_output: &mut usage_output,
                                        sse_tx: &sse_tx,
                                        item_id: &msg_id,
                                        sequence_number: &mut sequence_number,
                                    },
                                )
                                .await
                                {
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
                                            send_stream_error(
                                                &sse_tx,
                                                &mut sequence_number,
                                                "invalid_stream_data",
                                                "AI provider stream failed",
                                            )
                                            .await;
                                            meter_and_return!();
                                        }
                                    }
                                    EventResult::ProviderError => {
                                        send_stream_error(
                                            &sse_tx,
                                            &mut sequence_number,
                                            "provider_stream_error",
                                            "AI provider stream failed",
                                        )
                                        .await;
                                        meter_and_return!();
                                    }
                                    EventResult::ClientDisconnected => {
                                        meter_and_return!();
                                    }
                                    EventResult::UpstreamDone => {
                                        break 'upstream;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "AI provider stream failed");
                            send_stream_error(
                                &sse_tx,
                                &mut sequence_number,
                                "provider_stream_error",
                                "AI provider stream failed",
                            )
                            .await;
                            meter_and_return!();
                        }
                    }
                }
                _ = keepalive_interval.tick() => {
                    // Send SSE keepalive comment to prevent connection timeout
                    if sse_tx.send(Ok(Bytes::from(": keepalive\n\n"))).await.is_err() {
                        meter_and_return!();
                    }
                }
            }
        }

        // Extract model name from upstream response if available.
        // The last upstream chunk may contain model/usage info.
        // For OpenAI streaming, the final usage chunk includes model.
        // For Anthropic streaming, model is in message_start event.
        let final_model = model_name.unwrap_or(selected_model);

        // A pure safety refusal produces no ordinary text; represent it as a
        // refusal content part (mirroring the non-streaming translator) rather
        // than an empty output_text. If any text was produced, text wins.
        let is_refusal = text_accumulator.is_empty() && !refusal_accumulator.is_empty();

        // Estimate output tokens from whatever the model produced (text or
        // refusal) when the provider didn't report usage.
        if usage_output == 0 {
            let produced = text_accumulator.len().max(refusal_accumulator.len());
            if produced > 0 {
                usage_output = (produced as u64 / 4).max(1);
            }
        }

        // The upstream may never emit a prompt-usage frame (a provider that
        // omits stream usage, or a stream that ended before the usage chunk).
        // Fall back to the request's character-based input estimate so the
        // input side is billed rather than silently recorded as zero.
        if usage_input == 0 {
            usage_input = input_token_estimate;
        }

        let response_status = termination.response_status();
        let incomplete_details = termination
            .incomplete_reason()
            .map(|reason| serde_json::json!({"reason": reason}));
        let response_error = if response_status == "failed" {
            failed_stream_response_error()
        } else {
            Value::Null
        };

        let output_part = if is_refusal {
            serde_json::json!({
                "type": "refusal",
                "refusal": refusal_accumulator
            })
        } else {
            serde_json::json!({
                "type": "output_text",
                "text": text_accumulator,
                "annotations": []
            })
        };
        let output_item = serde_json::json!({
            "id": msg_id,
            "type": "message",
            "status": if response_status == "completed" { "completed" } else { "incomplete" },
            "role": "assistant",
            "content": [output_part]
        });
        let response_payload = serde_json::json!({
            "id": response_id,
            "object": "response",
            "created_at": created_at,
            "model": final_model,
            "capability": capability_name,
            "status": response_status,
            "output": [output_item],
            "usage": {
                "input_tokens": usage_input,
                "output_tokens": usage_output,
                "total_tokens": usage_input + usage_output
            },
            "incomplete_details": incomplete_details,
            "error": response_error
        });

        let (done_event_name, done_event) = if is_refusal {
            (
                "response.refusal.done",
                sse_events::refusal_done(&refusal_accumulator, &msg_id, sequence_number),
            )
        } else {
            (
                "response.output_text.done",
                serde_json::json!({
                    "type": "response.output_text.done",
                    "item_id": msg_id,
                    "output_index": 0,
                    "content_index": 0,
                    "text": text_accumulator,
                    "sequence_number": sequence_number
                }),
            )
        };
        sequence_number += 1;
        let mut downstream_open = sse_tx
            .send(Ok(Bytes::from(sse_events::format_sse_event(
                done_event_name,
                &done_event,
            ))))
            .await
            .is_ok();

        let part_done = serde_json::json!({
            "type": "response.content_part.done",
            "item_id": msg_id,
            "output_index": 0,
            "content_index": 0,
            "part": output_part,
            "sequence_number": sequence_number
        });
        sequence_number += 1;
        if downstream_open {
            downstream_open = sse_tx
                .send(Ok(Bytes::from(sse_events::format_sse_event(
                    "response.content_part.done",
                    &part_done,
                ))))
                .await
                .is_ok();
        }

        let item_done = serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": output_item,
            "sequence_number": sequence_number
        });
        sequence_number += 1;
        if downstream_open {
            downstream_open = sse_tx
                .send(Ok(Bytes::from(sse_events::format_sse_event(
                    "response.output_item.done",
                    &item_done,
                ))))
                .await
                .is_ok();
        }

        let terminal_event_name = termination.terminal_event_name();
        let terminal_event = serde_json::json!({
            "type": terminal_event_name,
            "response": response_payload,
            "sequence_number": sequence_number
        });
        if downstream_open {
            let terminal_str = sse_events::format_sse_event(terminal_event_name, &terminal_event);
            let _ = sse_tx.send(Ok(Bytes::from(terminal_str))).await;
        }
        // Close the provider and client streams immediately after the terminal
        // event. Metering cannot retain either network connection.
        drop(upstream_rx);
        drop(sse_tx);

        record_stream_usage(
            &metering,
            &meter_user_id,
            &meter_tenant_id,
            channel_id,
            &final_model,
            &capability_name,
            &response_payload,
        )
        .await;
    });

    let mut response = Response::sse(sse_rx);
    // Disable buffering in Nginx-compatible reverse proxies. Without this,
    // valid SSE events may be held until the proxy buffer fills.
    response.insert_header("x-accel-buffering", "no");
    Ok(response)
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
    /// The provider sent an in-band error event.
    ProviderError,
    /// The provider sent its explicit terminal marker.
    UpstreamDone,
}

/// A malformed provider event may contain lost output, so fail immediately
/// instead of silently skipping it and later declaring the response complete.
const MAX_CONSECUTIVE_PARSE_ERRORS: u32 = 1;

struct UpstreamEventContext<'a> {
    text_accumulator: &'a mut String,
    refusal_accumulator: &'a mut String,
    model_name: &'a mut Option<String>,
    usage_input: &'a mut u64,
    usage_output: &'a mut u64,
    sse_tx: &'a tokio::sync::mpsc::Sender<Result<Bytes, String>>,
    item_id: &'a str,
    sequence_number: &'a mut u64,
}

/// Process a single upstream SSE event string and forward translated events.
async fn process_upstream_event(
    event_str: &str,
    context: &mut UpstreamEventContext<'_>,
) -> EventResult {
    // Skip empty blocks. A block that begins with an SSE comment (`:`) is NOT
    // skipped wholesale — it may still carry a `data:` line, which
    // `upstream_event_data` extracts while ignoring the comment.
    if event_str.is_empty() {
        return EventResult::Continue;
    }
    if upstream_event_type(event_str) == Some("error") {
        tracing::warn!("AI provider returned an in-band stream error");
        return EventResult::ProviderError;
    }

    // Extract data from SSE event (data: {...})
    let data = match upstream_event_data(event_str) {
        Some(data) => data,
        None => return EventResult::Continue,
    };

    // Skip [DONE] signal
    if data == "[DONE]" {
        return EventResult::UpstreamDone;
    }

    // Parse JSON with error logging
    let chunk: Value = match serde_json::from_str(&data) {
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

    if chunk.get("error").is_some_and(|error| !error.is_null())
        || matches!(chunk.get("type").and_then(Value::as_str), Some("error"))
        || matches!(chunk.get("object").and_then(Value::as_str), Some("error"))
    {
        tracing::warn!("AI provider returned an in-band stream error");
        return EventResult::ProviderError;
    }

    // Extract and translate delta — this also populates model_name and usage
    let result = translate_streaming_chunk(
        &chunk,
        context.text_accumulator,
        context.model_name,
        context.usage_input,
        context.usage_output,
        context.item_id,
        *context.sequence_number,
    );

    if let Some((sse_str, _delta)) = result {
        *context.sequence_number = context.sequence_number.saturating_add(1);
        // If client disconnected (send fails), signal caller to stop
        if context.sse_tx.send(Ok(Bytes::from(sse_str))).await.is_err() {
            return EventResult::ClientDisconnected;
        }
    }

    // A safety refusal streams on `delta.refusal`, parallel to `delta.content`.
    // It is a successful completion carrying no ordinary text, so surface it as
    // a dedicated refusal event and accumulate it for the terminal output item
    // instead of letting it vanish into an empty response.
    if let Some(refusal) = crate::services::responses::extract_upstream_refusal(&chunk) {
        context.refusal_accumulator.push_str(&refusal);
        let event = sse_events::refusal_delta(&refusal, context.item_id, *context.sequence_number);
        *context.sequence_number = context.sequence_number.saturating_add(1);
        let sse_str = sse_events::format_sse_event("response.refusal.delta", &event);
        if context.sse_tx.send(Ok(Bytes::from(sse_str))).await.is_err() {
            return EventResult::ClientDisconnected;
        }
    }

    EventResult::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::channel::ModelCapability;
    use serde_json::json;

    async fn process_upstream_event(
        event_str: &str,
        text_accumulator: &mut String,
        model_name: &mut Option<String>,
        usage_input: &mut u64,
        usage_output: &mut u64,
        sse_tx: &tokio::sync::mpsc::Sender<Result<Bytes, String>>,
    ) -> EventResult {
        let mut sequence_number = 0;
        let mut refusal_accumulator = String::new();
        super::process_upstream_event(
            event_str,
            &mut UpstreamEventContext {
                text_accumulator,
                refusal_accumulator: &mut refusal_accumulator,
                model_name,
                usage_input,
                usage_output,
                sse_tx,
                item_id: "msg_test",
                sequence_number: &mut sequence_number,
            },
        )
        .await
    }

    #[test]
    fn parses_all_unified_capabilities() {
        let cases = [
            ("chat", ModelCapability::Chat),
            ("vision", ModelCapability::Vision),
            ("embedding", ModelCapability::Embedding),
            ("stt", ModelCapability::Stt),
            ("tts", ModelCapability::Tts),
        ];

        for (wire_name, expected) in cases {
            let body = json!({"capability": wire_name, "input": "test"});
            assert_eq!(parse_requested_capability(&body).unwrap(), Some(expected));
        }
    }

    #[test]
    fn missing_capability_enables_content_detection() {
        assert_eq!(
            parse_requested_capability(&json!({"input": "test"})).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_unknown_or_non_string_capability() {
        assert!(parse_requested_capability(&json!({"capability": "image_generation"})).is_err());
        assert!(parse_requested_capability(&json!({"capability": 42})).is_err());
    }

    #[test]
    fn validates_optional_model_field() {
        assert!(validate_requested_model(&json!({"input": "hi"})).is_ok());
        assert!(validate_requested_model(&json!({"model": "gpt-4o"})).is_ok());
        assert!(validate_requested_model(&json!({"model": ""})).is_err());
        assert!(validate_requested_model(&json!({"model": "   "})).is_err());
        assert!(validate_requested_model(&json!({"model": 42})).is_err());
    }

    #[test]
    fn validates_generation_ranges_before_upstream_dispatch() {
        let valid: ResponsesRequest = serde_json::from_value(json!({
            "input": "hello",
            "temperature": 0.5,
            "max_output_tokens": 1
        }))
        .unwrap();
        assert!(validate_generation_options(&valid).is_ok());

        let zero_tokens: ResponsesRequest =
            serde_json::from_value(json!({"input": "hello", "max_output_tokens": 0})).unwrap();
        assert!(validate_generation_options(&zero_tokens).is_err());

        let invalid_temperature: ResponsesRequest =
            serde_json::from_value(json!({"input": "hello", "temperature": -0.1})).unwrap();
        assert!(validate_generation_options(&invalid_temperature).is_err());

        let protocol_ambiguous_temperature: ResponsesRequest =
            serde_json::from_value(json!({"input": "hello", "temperature": 1.5})).unwrap();
        assert!(validate_generation_options(&protocol_ambiguous_temperature).is_err());

        let unsupported_tools: ResponsesRequest = serde_json::from_value(json!({
            "input": "hello",
            "tools": [{"type": "web_search"}]
        }))
        .unwrap();
        assert!(validate_generation_options(&unsupported_tools).is_err());

        let unsupported_storage: ResponsesRequest =
            serde_json::from_value(json!({"input": "hello", "store": true})).unwrap();
        assert!(validate_generation_options(&unsupported_storage).is_err());

        let unsupported_previous: ResponsesRequest = serde_json::from_value(json!({
            "input": "hello",
            "previous_response_id": "resp_previous"
        }))
        .unwrap();
        assert!(validate_generation_options(&unsupported_previous).is_err());

        let stateless: ResponsesRequest =
            serde_json::from_value(json!({"input": "hello", "store": false})).unwrap();
        assert!(validate_generation_options(&stateless).is_ok());
    }

    #[test]
    fn direct_capabilities_accept_stateless_common_noops() {
        for capability in [
            ModelCapability::Embedding,
            ModelCapability::Stt,
            ModelCapability::Tts,
        ] {
            let mut object = json!({
                "stream": false,
                "store": false,
                "tools": [],
                "previous_response_id": null,
                "metadata": {"trace_id": "test"}
            })
            .as_object()
            .unwrap()
            .clone();

            normalize_direct_common_fields(&mut object)
                .unwrap_or_else(|error| panic!("{capability:?}: {}", error.message));
            assert!(object.is_empty(), "{capability:?}: {object:?}");
        }
    }

    #[test]
    fn direct_capabilities_reject_stateful_or_malformed_common_fields() {
        for body in [
            json!({"store": true}),
            json!({"store": "false"}),
            json!({"tools": [{"type": "web_search"}]}),
            json!({"tools": {}}),
            json!({"previous_response_id": "resp_previous"}),
            json!({"stream": true}),
            json!({"stream": "false"}),
            json!({"metadata": {"trace_id": 42}}),
        ] {
            let mut object = body.as_object().unwrap().clone();
            assert!(
                normalize_direct_common_fields(&mut object).is_err(),
                "body: {body}"
            );
        }
    }

    #[test]
    fn unknown_top_level_response_parameters_are_rejected() {
        assert!(
            serde_json::from_value::<ResponsesRequest>(json!({
                "input": "hello",
                "unsupported_parameter": true
            }))
            .is_err()
        );
    }

    #[test]
    fn handler_errors_use_the_unified_failed_envelope() {
        let response = failed_response(HttpError::bad_request("invalid request")).unwrap();
        assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(&response.read_bytes().unwrap()).unwrap();
        assert_eq!(body["object"], "response");
        assert_eq!(body["status"], "failed");
        assert_eq!(body["output"], json!([]));
        assert_eq!(body["incomplete_details"], Value::Null);
        assert_eq!(body["error"]["code"], "bad_request");
        assert_eq!(body["error"]["message"], "invalid request");
        assert!(body["error"].get("type").is_none());
        assert!(body["error"].get("param").is_none());
    }

    #[test]
    fn rejects_incompatible_stt_combinations() {
        assert!(
            validate_capability_combination(&[ModelCapability::Chat, ModelCapability::Stt]).is_ok()
        );
        assert!(
            validate_capability_combination(&[
                ModelCapability::Chat,
                ModelCapability::Vision,
                ModelCapability::Stt,
            ])
            .is_err()
        );
    }

    #[test]
    fn normalizes_unified_stt_input() {
        let mut object = json!({
            "input": {"data": "YWJj", "format": "mp3"},
            "model": "whisper-1"
        })
        .as_object()
        .unwrap()
        .clone();

        normalize_stt_body(&mut object).unwrap();

        assert_eq!(object["file"], "YWJj");
        assert_eq!(object["filename"], "audio.mp3");
        assert_eq!(object["model"], "whisper-1");
        assert!(!object.contains_key("input"));
    }

    #[test]
    fn rejects_stt_without_audio_data() {
        let mut object = json!({"input": {"format": "wav"}})
            .as_object()
            .unwrap()
            .clone();
        assert!(normalize_stt_body(&mut object).is_err());
    }

    #[test]
    fn normalizes_nested_responses_audio_input() {
        let mut object = json!({
            "input": [{
                "role": "user",
                "content": [{"type": "input_audio", "data": "YWJj", "format": "wav"}]
            }]
        })
        .as_object()
        .unwrap()
        .clone();

        normalize_stt_body(&mut object).unwrap();

        assert_eq!(object["file"], "YWJj");
        assert_eq!(object["filename"], "audio.wav");
    }

    #[test]
    fn rejects_multiple_stt_audio_parts() {
        let mut object = json!({
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_audio", "data": "YWJj", "format": "wav"},
                    {"type": "input_audio", "data": "ZGVm", "format": "wav"}
                ]
            }]
        })
        .as_object()
        .unwrap()
        .clone();

        assert!(normalize_stt_body(&mut object).is_err());
    }

    #[test]
    fn normalizes_unified_tts_audio_configuration() {
        let mut object = json!({
            "model": "gpt-4o-mini-tts",
            "input": "Hello",
            "audio": {
                "voice": "alloy",
                "format": "wav",
                "speed": 1.25,
                "instructions": "Speak warmly",
                "stream_format": "audio"
            }
        })
        .as_object()
        .unwrap()
        .clone();

        normalize_tts_body(&mut object).unwrap();

        assert_eq!(object["voice"], "alloy");
        assert_eq!(object["response_format"], "wav");
        assert_eq!(object["speed"], 1.25);
        assert_eq!(object["instructions"], "Speak warmly");
        assert_eq!(object["stream_format"], "audio");
        assert!(!object.contains_key("audio"));
    }

    #[test]
    fn preserves_valid_legacy_top_level_tts_configuration() {
        let mut object = json!({
            "input": "Hello",
            "voice": "alloy",
            "response_format": "mp3",
            "speed": 1.0
        })
        .as_object()
        .unwrap()
        .clone();

        normalize_tts_body(&mut object).unwrap();

        assert_eq!(object["voice"], "alloy");
        assert_eq!(object["response_format"], "mp3");
        assert_eq!(object["speed"], 1.0);
    }

    #[test]
    fn rejects_invalid_or_conflicting_tts_audio_configuration() {
        let invalid = [
            json!({"input": "Hello", "audio": {}}),
            json!({"input": "Hello", "audio": {"voice": "   "}}),
            json!({"input": "Hello", "audio": {"voice": 42}}),
            json!({"input": "Hello", "audio": {"voice": "alloy", "format": 42}}),
            json!({"input": "Hello", "audio": {"voice": "alloy", "speed": 4.1}}),
            json!({"input": "Hello", "audio": {"voice": "alloy", "stream_format": "sse"}}),
            json!({"input": "Hello", "audio": {"voice": "alloy", "unknown": true}}),
            json!({
                "input": "Hello",
                "voice": "alloy",
                "audio": {"voice": "nova"}
            }),
        ];

        for body in invalid {
            let mut object = body.as_object().unwrap().clone();
            assert!(normalize_tts_body(&mut object).is_err(), "body: {body}");
        }
    }

    #[test]
    fn preserves_complete_multimodal_capability_set() {
        let detected = vec![ModelCapability::Chat, ModelCapability::Vision];
        assert_eq!(
            required_capabilities(&detected, Some(&ModelCapability::Vision)),
            detected
        );
    }

    #[test]
    fn direct_input_validation_rejects_invalid_embedding_and_tts() {
        let empty = serde_json::Map::new();
        assert!(validate_direct_input(&ModelCapability::Embedding, &empty).is_err());
        assert!(validate_direct_input(&ModelCapability::Tts, &empty).is_err());

        let valid_embedding = json!({"input": ["one", "two"]})
            .as_object()
            .unwrap()
            .clone();
        assert!(validate_direct_input(&ModelCapability::Embedding, &valid_embedding).is_ok());
    }

    #[test]
    fn embedding_contract_accepts_base64_and_rejects_invalid_dimensions() {
        for body in [
            json!({"input": "hello", "dimensions": 0}),
            json!({"input": "hello", "dimensions": "1536"}),
        ] {
            assert!(
                validate_direct_fields(&ModelCapability::Embedding, body.as_object().unwrap(),)
                    .is_err(),
                "body: {body}"
            );
        }
        let base64 = json!({"input": "hello", "encoding_format": "base64"});
        assert!(
            validate_direct_fields(&ModelCapability::Embedding, base64.as_object().unwrap())
                .is_ok()
        );
    }

    #[test]
    fn base64_embedding_is_normalized_to_a_float_vector() {
        let mut result = json!({
            "data": [{"embedding": "AACAPwAAAMA=", "index": 0}]
        });
        normalize_and_validate_direct_result(&ModelCapability::Embedding, &mut result).unwrap();
        assert_eq!(result["data"][0]["embedding"], json!([1.0, -2.0]));
        assert_eq!(result["data"][0]["dimensions"], 2);

        let mut invalid = json!({"data": [{"embedding": "not-base64"}]});
        assert!(
            normalize_and_validate_direct_result(&ModelCapability::Embedding, &mut invalid)
                .is_err()
        );
    }

    #[test]
    fn stt_contract_accepts_and_validates_forwarded_options() {
        let valid = json!({
            "model": "whisper-1",
            "file": "YWJj",
            "filename": "audio.wav",
            "language": "zh",
            "prompt": "AINS",
            "response_format": "json",
            "temperature": 0.25
        });
        assert!(validate_direct_fields(&ModelCapability::Stt, valid.as_object().unwrap()).is_ok());

        let invalid = json!({
            "file": "YWJj",
            "filename": "audio.wav",
            "temperature": 1.5
        });
        assert!(
            validate_direct_fields(&ModelCapability::Stt, invalid.as_object().unwrap()).is_err()
        );

        let invalid_base64 = json!({
            "file": "not base64!",
            "filename": "audio.wav"
        });
        assert!(
            validate_direct_fields(&ModelCapability::Stt, invalid_base64.as_object().unwrap())
                .is_err()
        );

        for extension in ["mp4", "mpeg", "mpga"] {
            let body = json!({
                "file": "YWJj",
                "filename": format!("audio.{extension}"),
                "response_format": "text"
            });
            assert!(
                validate_direct_fields(&ModelCapability::Stt, body.as_object().unwrap()).is_ok(),
                "extension: {extension}"
            );
        }
    }

    #[test]
    fn direct_capabilities_use_unified_response_envelope() {
        let result = json!({
            "model": "text-embedding-3-small",
            "data": [{
                "object": "embedding",
                "embedding": [0.1, 0.2],
                "index": 0,
                "provider_extension": "preserved"
            }],
            "usage": {"prompt_tokens": 2, "total_tokens": 2}
        });

        let response = unified_direct_response(&ModelCapability::Embedding, &result);

        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["incomplete_details"], Value::Null);
        assert_eq!(response["capability"], "embedding");
        assert_eq!(response["model"], "text-embedding-3-small");
        assert!(response["output"].is_array());
        assert_eq!(response["output"][0]["dimensions"], 2);
        assert_eq!(response["output"][0]["object"], "embedding");
        assert_eq!(response["output"][0]["provider_extension"], "preserved");
        assert_eq!(response["usage"]["input_tokens"], 2);
        assert_eq!(response["usage"]["output_tokens"], 0);
        assert_eq!(response["usage"]["total_tokens"], 2);
    }

    #[test]
    fn malformed_provider_results_are_rejected() {
        assert!(validate_chat_result(&json!({})).is_err());
        // Representable assistant text is valid regardless of finish_reason —
        // a null/absent finish_reason must NOT be treated as malformed (doing
        // so previously produced a spurious 503 that tripped the breaker).
        assert!(
            validate_chat_result(&json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "partial"},
                    "finish_reason": null
                }]
            }))
            .is_ok()
        );
        // Genuinely malformed: no text, no refusal, and not a content filter.
        assert!(
            validate_chat_result(&json!({
                "choices": [{
                    "message": {"role": "assistant", "content": null},
                    "finish_reason": null
                }]
            }))
            .is_err()
        );
        assert!(
            validate_chat_result(&json!({
                "choices": [{
                    "message": {"role": "assistant", "content": null},
                    "finish_reason": "content_filter"
                }]
            }))
            .is_ok()
        );
        assert!(
            validate_chat_result(&json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "refusal": "I cannot help with that."
                    },
                    "finish_reason": "stop"
                }]
            }))
            .is_ok()
        );

        for (capability, mut result) in [
            (ModelCapability::Embedding, json!({"data": []})),
            (ModelCapability::Stt, json!({"model": "whisper-1"})),
            (ModelCapability::Tts, json!({"audio": ""})),
        ] {
            assert!(normalize_and_validate_direct_result(&capability, &mut result).is_err());
        }
    }

    #[test]
    fn buffers_split_utf8_until_a_complete_sse_event() {
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n";
        let split = event.find('你').unwrap() + 1;
        let mut buffer = event.as_bytes()[..split].to_vec();

        assert!(take_next_sse_event(&mut buffer).is_none());
        buffer.extend_from_slice(&event.as_bytes()[split..]);

        assert_eq!(
            take_next_sse_event(&mut buffer).unwrap().unwrap(),
            event.trim_end_matches("\n\n")
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn accepts_crlf_sse_event_delimiters() {
        let mut buffer = b"event: ping\r\ndata: {}\r\n\r\nnext".to_vec();
        assert_eq!(
            take_next_sse_event(&mut buffer).unwrap().unwrap(),
            "event: ping\r\ndata: {}"
        );
        assert_eq!(buffer, b"next");
    }

    #[test]
    fn stream_requires_stop_and_done_to_complete() {
        let mut completed = StreamTerminationState::default();
        completed.observe(r#"data: {"choices":[{"finish_reason":"stop","delta":{}}]}"#);
        assert!(!completed.is_completed());
        assert_eq!(completed.terminal_event_name(), "response.failed");
        completed.observe("data: [DONE]");
        assert!(completed.is_completed());
        assert_eq!(completed.terminal_event_name(), "response.completed");

        let mut length = StreamTerminationState::default();
        length.observe(r#"data: {"choices":[{"finish_reason":"length","delta":{}}]}"#);
        length.observe("data: [DONE]");
        assert!(!length.is_completed());
        assert_eq!(length.incomplete_reason(), Some("max_output_tokens"));
        assert_eq!(length.terminal_event_name(), "response.incomplete");

        let mut eof = StreamTerminationState::default();
        eof.observe(r#"data: {"choices":[{"finish_reason":"stop","delta":{}}]}"#);
        assert_eq!(eof.incomplete_reason(), None);
        assert_eq!(eof.terminal_event_name(), "response.failed");
    }

    #[test]
    fn accepts_sse_data_fields_without_a_space() {
        assert_eq!(
            upstream_event_data("event: ping\ndata:{\"ok\":true}").as_deref(),
            Some("{\"ok\":true}")
        );
        let mut state = StreamTerminationState::default();
        state.observe("data:[DONE]");
        assert!(state.saw_done);
    }

    #[test]
    fn partial_stream_payload_preserves_observed_or_estimated_usage() {
        // Observed prompt usage wins over the input estimate.
        let observed = partial_stream_metering_payload("gpt", "chat", 4, 2, 100, "hello");
        assert_eq!(observed["usage"]["input_tokens"], 4);
        assert_eq!(observed["usage"]["output_tokens"], 2);

        // No observed usage: input falls back to the request estimate and
        // output is estimated from the accumulated text.
        let estimated = partial_stream_metering_payload("gpt", "chat", 0, 0, 7, "abcdefgh");
        assert_eq!(estimated["usage"]["input_tokens"], 7);
        assert_eq!(estimated["usage"]["output_tokens"], 2);
        assert_eq!(estimated["status"], "failed");

        let anthropic_partial =
            partial_stream_metering_payload("claude", "chat", 11, 0, 100, "abcdefgh");
        assert_eq!(anthropic_partial["usage"]["input_tokens"], 11);
        assert_eq!(anthropic_partial["usage"]["output_tokens"], 2);
    }

    #[test]
    fn failed_response_error_uses_the_response_error_schema() {
        let error = failed_stream_response_error();
        assert_eq!(error["code"], "server_error");
        assert_eq!(error["message"], "AI provider stream failed");
        assert!(error.get("type").is_none());
        assert!(error.get("param").is_none());
    }

    /// Normal SSE event with a text delta — should be forwarded.
    #[tokio::test]
    async fn process_upstream_event_normal_delta() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let event =
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let result =
            process_upstream_event(event, &mut acc, &mut model, &mut inp, &mut out, &tx).await;
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(!acc.is_empty(), "accumulator should contain delta");
        assert!(rx.try_recv().is_ok(), "should have sent an SSE event");
    }

    /// A streamed safety refusal (`delta.refusal`) must be accumulated and
    /// surfaced as a `response.refusal.delta` event, not silently dropped.
    #[tokio::test]
    async fn process_upstream_event_refusal_delta() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut text_acc = String::new();
        let mut refusal_acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;
        let mut seq = 0u64;

        let event = r#"data: {"choices":[{"index":0,"delta":{"refusal":"I cannot"},"finish_reason":null}]}"#;
        let result = super::process_upstream_event(
            event,
            &mut UpstreamEventContext {
                text_accumulator: &mut text_acc,
                refusal_accumulator: &mut refusal_acc,
                model_name: &mut model,
                usage_input: &mut inp,
                usage_output: &mut out,
                sse_tx: &tx,
                item_id: "msg_test",
                sequence_number: &mut seq,
            },
        )
        .await;
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(text_acc.is_empty(), "refusal must not populate text");
        assert_eq!(refusal_acc, "I cannot");
        let sent = rx.try_recv().expect("a refusal SSE event should be sent");
        let sent = sent.expect("event should be Ok bytes");
        let sent = String::from_utf8(sent.to_vec()).unwrap();
        assert!(sent.contains("response.refusal.delta"));
    }

    /// Empty event string — should be skipped silently.
    #[tokio::test]
    async fn process_upstream_event_empty_string() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let result =
            process_upstream_event("", &mut acc, &mut model, &mut inp, &mut out, &tx).await;
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(acc.is_empty());
        assert!(_rx.is_empty());
    }

    /// Comment line (starts with :) — should be skipped.
    #[tokio::test]
    async fn process_upstream_event_comment_line() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let result =
            process_upstream_event(": keepalive", &mut acc, &mut model, &mut inp, &mut out, &tx)
                .await;
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(_rx.is_empty());
    }

    /// [DONE] signal — should be skipped (no event sent).
    #[tokio::test]
    async fn process_upstream_event_done_signal() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
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
        )
        .await;
        drop(tx);

        assert!(matches!(result, EventResult::UpstreamDone));
        assert!(_rx.is_empty());
    }

    #[tokio::test]
    async fn stream_errors_are_visible_and_do_not_leak_internal_details() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut sequence_number = 7;

        send_stream_error(
            &tx,
            &mut sequence_number,
            "provider_stream_error",
            "AI provider stream failed",
        )
        .await;

        let event = String::from_utf8(rx.recv().await.unwrap().unwrap().to_vec()).unwrap();
        assert!(event.contains("event: error"));
        assert!(event.contains("AI provider stream failed"));
        assert!(event.contains("\"sequence_number\":7"));
        assert!(!event.contains("https://"));
        assert_eq!(sequence_number, 8);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_error_notification_is_bounded_when_client_never_drains() {
        // A client that never drains must not pin the terminal send forever.
        // The bounded timeout lets cleanup proceed even though the event could
        // not be delivered. Virtual time (start_paused) fires the timeout
        // without a real wall-clock wait.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(b"queued"))).await.unwrap();
        let mut sequence_number = 0;

        send_stream_error(
            &tx,
            &mut sequence_number,
            "provider_stream_error",
            "AI provider stream failed",
        )
        .await;
        assert_eq!(sequence_number, 1);
    }

    /// Malformed JSON in the data field — should return ParseError.
    #[tokio::test]
    async fn process_upstream_event_malformed_json() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
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
        )
        .await;
        drop(tx);

        assert!(matches!(result, EventResult::ParseError));
        assert!(_rx.is_empty());
    }

    #[tokio::test]
    async fn process_upstream_event_detects_in_band_provider_errors() {
        for event in [
            "event:error\ndata:{\"message\":\"failed\"}",
            "data:{\"error\":{\"message\":\"failed\"}}",
        ] {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            let mut acc = String::new();
            let mut model = None;
            let mut input = 0;
            let mut output = 0;
            let result =
                process_upstream_event(event, &mut acc, &mut model, &mut input, &mut output, &tx)
                    .await;
            assert!(matches!(result, EventResult::ProviderError));
        }
    }

    /// Event with no "data:" line — should be skipped.
    #[tokio::test]
    async fn process_upstream_event_no_data_line() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
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
        )
        .await;
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert!(_rx.is_empty());
    }

    /// Client disconnects (receiver dropped) — should return ClientDisconnected.
    #[tokio::test]
    async fn process_upstream_event_client_disconnected() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        drop(rx); // Drop receiver before sending

        let event =
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let result =
            process_upstream_event(event, &mut acc, &mut model, &mut inp, &mut out, &tx).await;

        assert!(matches!(result, EventResult::ClientDisconnected));
    }

    /// Event with usage data updates input/output counters.
    #[tokio::test]
    async fn process_upstream_event_with_usage_updates_counters() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let event = r#"data: {"usage":{"prompt_tokens":10,"completion_tokens":20},"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let result =
            process_upstream_event(event, &mut acc, &mut model, &mut inp, &mut out, &tx).await;
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert_eq!(inp, 10);
        assert_eq!(out, 20);
    }

    // ── Contract/validation regression tests ────────────────────────

    #[test]
    fn rejects_input_file_until_preprocessing_exists() {
        // input_file contents are never sent upstream; the endpoint must reject
        // it rather than fabricate a "file available" placeholder.
        let req: ResponsesRequest = serde_json::from_value(json!({
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "summarise"},
                    {"type": "input_file", "file_data": "JVBERi0=", "filename": "a.pdf"}
                ]
            }]
        }))
        .expect("request should deserialize");
        assert!(
            translate_request(&req).is_err(),
            "input_file must be rejected until real extraction/upload exists"
        );
    }

    #[test]
    fn tts_rejects_unsupported_format_before_dispatch() {
        let mut object = serde_json::Map::new();
        object.insert("voice".into(), json!("alloy"));
        object.insert("response_format".into(), json!("avi"));
        let err = normalize_tts_body(&mut object).expect_err("avi must be rejected");
        assert_eq!(err.status, http::StatusCode::BAD_REQUEST);

        let mut ok = serde_json::Map::new();
        ok.insert("voice".into(), json!("alloy"));
        ok.insert("response_format".into(), json!("MP3"));
        assert!(
            normalize_tts_body(&mut ok).is_ok(),
            "supported formats (case-insensitive) must pass"
        );
    }

    #[test]
    fn stt_rejects_mixed_image_and_audio_input() {
        let mut object = serde_json::Map::new();
        object.insert(
            "input".into(),
            json!([{
                "role": "user",
                "content": [
                    {"type": "input_audio", "data": "AAAA", "format": "wav"},
                    {"type": "input_image", "image_url": "https://example.com/x.png"}
                ]
            }]),
        );
        let err = normalize_stt_body(&mut object).expect_err("mixed image+audio must be rejected");
        assert_eq!(err.status, http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn upstream_event_data_concatenates_multiple_data_lines() {
        // Two data: lines are joined with a newline (SSE spec), matching the
        // gateway parser. Reading only the first line would corrupt JSON.
        let event = "data: {\"a\":1,\ndata: \"b\":2}";
        assert_eq!(
            upstream_event_data(event).as_deref(),
            Some("{\"a\":1,\n\"b\":2}")
        );
    }

    #[tokio::test]
    async fn process_upstream_event_forwards_data_after_leading_comment() {
        // A block that begins with an SSE comment must still have its data
        // event processed rather than dropped wholesale.
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut acc = String::new();
        let mut model = None;
        let mut inp = 0;
        let mut out = 0;

        let event = ": keepalive\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}";
        let result =
            process_upstream_event(event, &mut acc, &mut model, &mut inp, &mut out, &tx).await;
        drop(tx);

        assert!(matches!(result, EventResult::Continue));
        assert_eq!(acc, "Hi", "delta after a leading comment must be processed");
        assert!(rx.try_recv().is_ok(), "a translated event should be sent");
    }

    #[tokio::test]
    async fn send_stream_error_delivers_terminal_event_to_briefly_full_queue() {
        // A momentarily-full client queue must not drop the terminal `error`
        // event; otherwise a failed stream is indistinguishable from a clean
        // EOF. The send waits for the client to drain instead of best-effort
        // dropping on `Full`.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(1);
        // Saturate the queue so a bare `try_send` would discard the error.
        tx.send(Ok(Bytes::from_static(b"pending-delta")))
            .await
            .unwrap();

        let sender = tx.clone();
        let error_task = tokio::spawn(async move {
            let mut sequence_number = 7;
            send_stream_error(
                &sender,
                &mut sequence_number,
                "provider_stream_error",
                "AI provider stream failed",
            )
            .await;
        });

        // Drain the pre-existing delta so the terminal error has room.
        let first = rx.recv().await.unwrap().unwrap();
        assert_eq!(first, Bytes::from_static(b"pending-delta"));

        error_task.await.unwrap();

        let terminal = rx.recv().await.unwrap().unwrap();
        let text = String::from_utf8(terminal.to_vec()).unwrap();
        assert!(text.contains("event: error"), "terminal event: {text}");
        assert!(
            text.contains("provider_stream_error"),
            "terminal event: {text}"
        );
    }
}
