//! Capability-based dispatch for AI Gateway proxy requests.
//!
//! Determines the correct upstream URL suffix, request format, and response
//! parsing strategy for each combination of (capability, protocol_type).

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};

use crate::repositories::channel::ModelCapability;
use crate::services::gateway::GatewayError;

/// The outcome of dispatching a proxy request: what URL to hit and how to
/// handle the request/response lifecycle.
#[derive(Debug)]
pub enum DispatchAction {
    /// Standard JSON POST with JSON response (Chat, Vision, Embedding).
    JsonPost {
        url: String,
        body: serde_json::Value,
    },
    /// Binary audio response from a JSON POST (TTS).
    TtsBinary {
        url: String,
        body: serde_json::Value,
    },
    /// Multipart upload for STT (audio file) with JSON response.
    SttMultipart {
        url: String,
        audio_bytes: Vec<u8>,
        filename: String,
        model: String,
        form_fields: Vec<(String, String)>,
    },
}

/// Determine how to proxy a request based on capability and protocol type.
///
/// Returns the `DispatchAction` that describes how to build and send the
/// upstream request.
pub fn dispatch_proxy(
    capability: &ModelCapability,
    protocol_type: &str,
    base_url: &str,
    body: serde_json::Value,
) -> Result<DispatchAction, GatewayError> {
    let base = base_url.trim_end_matches('/');

    match capability {
        ModelCapability::Chat | ModelCapability::Vision | ModelCapability::WebSearch => {
            match protocol_type {
                "openai" => Ok(DispatchAction::JsonPost {
                    url: format!("{}/v1/chat/completions", base),
                    body,
                }),
                "anthropic" => {
                    if let Some(temperature) = body.get("temperature")
                        && !temperature
                            .as_f64()
                            .is_some_and(|temperature| (0.0..=1.0).contains(&temperature))
                    {
                        return Err(GatewayError::InvalidInput(
                            "Anthropic temperature must be between 0 and 1".into(),
                        ));
                    }
                    // Anthropic /v1/messages uses a different request format;
                    // the request body transformation happens in the anthropic
                    // proxy layer. For dispatch we just set the URL, and the
                    // caller (GatewayService::proxy) handles body translation.
                    Ok(DispatchAction::JsonPost {
                        url: format!("{}/v1/messages", base),
                        body,
                    })
                }
                other => Err(GatewayError::InvalidInput(format!(
                    "Unsupported protocol type for Chat/Vision: {}",
                    other
                ))),
            }
        }
        ModelCapability::Embedding => match protocol_type {
            "openai" => Ok(DispatchAction::JsonPost {
                url: format!("{}/v1/embeddings", base),
                body,
            }),
            _ => Err(GatewayError::InvalidInput(format!(
                "Embedding only supports OpenAI protocol (got: {})",
                protocol_type
            ))),
        },
        ModelCapability::Stt if protocol_type == "openai" => {
            // STT: multipart upload — extract audio bytes from the request body.
            // The JSON contract carries audio as standard or URL-safe base64.
            let raw = body.get("file").and_then(|v| v.as_str()).ok_or_else(|| {
                GatewayError::InvalidInput(
                    "STT request must include a 'file' field with audio data".into(),
                )
            })?;
            let audio_bytes = URL_SAFE_NO_PAD
                .decode(raw)
                .or_else(|_| STANDARD.decode(raw))
                .map_err(|err| {
                    tracing::warn!(
                        "STT file field is not valid base64 ({}); rejecting request",
                        err,
                    );
                    GatewayError::InvalidInput(
                        "STT file field must be valid base64-encoded audio data".into(),
                    )
                })?;

            let filename = body
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("audio.wav")
                .to_string();

            let model = body
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("whisper-1")
                .to_string();

            let mut form_fields = Vec::new();
            for field in ["language", "prompt", "response_format"] {
                if let Some(value) = body.get(field) {
                    let value = value.as_str().ok_or_else(|| {
                        GatewayError::InvalidInput(format!("STT {field} must be a string"))
                    })?;
                    form_fields.push((field.to_string(), value.to_string()));
                }
            }
            if let Some(value) = body.get("temperature") {
                let value = value.as_f64().ok_or_else(|| {
                    GatewayError::InvalidInput("STT temperature must be a number".into())
                })?;
                form_fields.push(("temperature".into(), value.to_string()));
            }

            Ok(DispatchAction::SttMultipart {
                url: format!("{}/v1/audio/transcriptions", base),
                audio_bytes,
                filename,
                model,
                form_fields,
            })
        }
        ModelCapability::Stt => Err(GatewayError::InvalidInput(format!(
            "STT only supports OpenAI protocol (got: {})",
            protocol_type
        ))),
        ModelCapability::Tts => match protocol_type {
            "openai" => Ok(DispatchAction::TtsBinary {
                url: format!("{}/v1/audio/speech", base),
                body,
            }),
            _ => Err(GatewayError::InvalidInput(format!(
                "TTS only supports OpenAI protocol (got: {})",
                protocol_type
            ))),
        },
    }
}

/// Extract the `model` field from a JSON body (used by all proxy paths).
pub fn extract_model(body: &serde_json::Value) -> Option<String> {
    body.get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dispatch_openai_chat() {
        let body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        let action = dispatch_proxy(
            &ModelCapability::Chat,
            "openai",
            "https://api.openai.com",
            body.clone(),
        )
        .unwrap();
        match action {
            DispatchAction::JsonPost { url, body: b } => {
                assert_eq!(url, "https://api.openai.com/v1/chat/completions");
                assert_eq!(b, body);
            }
            _ => panic!("expected JsonPost"),
        }
    }

    #[test]
    fn dispatch_openai_embedding() {
        let body = json!({"model": "text-embedding-3-small", "input": "hello"});
        let action = dispatch_proxy(
            &ModelCapability::Embedding,
            "openai",
            "https://api.openai.com",
            body.clone(),
        )
        .unwrap();
        match action {
            DispatchAction::JsonPost { url, .. } => {
                assert_eq!(url, "https://api.openai.com/v1/embeddings");
            }
            _ => panic!("expected JsonPost"),
        }
    }

    #[test]
    fn dispatch_anthropic_chat() {
        let body = json!({"model": "claude-3-opus-20240229", "messages": [{"role": "user", "content": "hi"}]});
        let action = dispatch_proxy(
            &ModelCapability::Chat,
            "anthropic",
            "https://api.anthropic.com",
            body.clone(),
        )
        .unwrap();
        match action {
            DispatchAction::JsonPost { url, .. } => {
                assert_eq!(url, "https://api.anthropic.com/v1/messages");
            }
            _ => panic!("expected JsonPost"),
        }
    }

    #[test]
    fn dispatch_anthropic_rejects_out_of_range_temperature() {
        let body = json!({
            "model": "claude-3-opus-20240229",
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 1.5
        });

        assert!(matches!(
            dispatch_proxy(
                &ModelCapability::Chat,
                "anthropic",
                "https://api.anthropic.com",
                body,
            ),
            Err(GatewayError::InvalidInput(message))
                if message.contains("temperature")
        ));
    }

    #[test]
    fn dispatch_openai_tts() {
        let body = json!({"model": "tts-1", "input": "hello", "voice": "alloy"});
        let action = dispatch_proxy(
            &ModelCapability::Tts,
            "openai",
            "https://api.openai.com",
            body.clone(),
        )
        .unwrap();
        match action {
            DispatchAction::TtsBinary { url, .. } => {
                assert_eq!(url, "https://api.openai.com/v1/audio/speech");
            }
            _ => panic!("expected TtsBinary"),
        }
    }

    #[test]
    fn dispatch_unsupported_protocol_returns_error() {
        let body = json!({"model": "test"});
        let result = dispatch_proxy(
            &ModelCapability::Embedding,
            "anthropic",
            "https://api.test.com",
            body,
        );
        assert!(result.is_err());
    }

    #[test]
    fn dispatch_stt_missing_file_returns_error() {
        let body = json!({"model": "whisper-1"});
        let result = dispatch_proxy(
            &ModelCapability::Stt,
            "openai",
            "https://api.openai.com",
            body,
        );
        assert!(result.is_err());
    }

    #[test]
    fn dispatch_openai_stt_preserves_supported_form_fields() {
        let body = json!({
            "model": "whisper-1",
            "file": "YWJj",
            "filename": "audio.wav",
            "language": "zh",
            "prompt": "AINS",
            "response_format": "json",
            "temperature": 0.25
        });
        let action = dispatch_proxy(
            &ModelCapability::Stt,
            "openai",
            "https://api.openai.com",
            body,
        )
        .unwrap();

        let DispatchAction::SttMultipart { form_fields, .. } = action else {
            panic!("expected SttMultipart");
        };
        assert!(form_fields.contains(&("language".into(), "zh".into())));
        assert!(form_fields.contains(&("prompt".into(), "AINS".into())));
        assert!(form_fields.contains(&("response_format".into(), "json".into())));
        assert!(form_fields.contains(&("temperature".into(), "0.25".into())));
    }

    #[test]
    fn dispatch_anthropic_stt_returns_error() {
        let body = json!({"model": "whisper-1", "file": "YWJj"});
        let result = dispatch_proxy(
            &ModelCapability::Stt,
            "anthropic",
            "https://api.anthropic.com",
            body,
        );

        assert!(
            matches!(result, Err(GatewayError::InvalidInput(message)) if message.contains("STT only supports OpenAI"))
        );
    }
}
