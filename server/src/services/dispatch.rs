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

/// Join a configured channel base URL with an upstream API path suffix.
///
/// Tolerates base URLs that already carry the version prefix or a full endpoint
/// path — as commonly copied verbatim from OpenAI-compatible provider docs.
/// A full endpoint is normalized back to its API root before applying the
/// requested suffix, so a channel supporting Chat and Embedding does not turn
/// `.../v1/chat/completions` into `.../v1/chat/completions/v1/embeddings`.
/// Query parameters remain attached to the upstream URL; fragments are
/// client-side only and are discarded.
fn join_api_path(base_url: &str, suffix: &str) -> Result<String, GatewayError> {
    let mut url = reqwest::Url::parse(base_url).map_err(|error| {
        GatewayError::InvalidInput(format!("base_url must be a valid http(s) URL: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(GatewayError::InvalidInput(
            "base_url must be a valid http(s) URL without credentials".into(),
        ));
    }
    // Fragments never participate in an HTTP request. Keeping one while
    // changing the path would make the dispatched URL misleading in logs.
    url.set_fragment(None);

    let mut base = url.path().trim_end_matches('/').to_string();
    let suffix = format!("/{}", suffix.trim_start_matches('/'));
    // A configured full endpoint is only a convenience spelling for this
    // channel's API root. Strip any known endpoint before applying this call's
    // suffix; otherwise a multi-capability channel routes every non-matching
    // capability beneath the first endpoint path.
    for endpoint in [
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/embeddings",
        "/v1/audio/transcriptions",
        "/v1/audio/speech",
    ] {
        if let Some(root) = base.strip_suffix(endpoint) {
            base = root.to_string();
            break;
        }
    }
    // Base already ends with the version segment that the suffix also
    // starts with (e.g. base `.../v1` + suffix `v1/chat/completions`).
    if let Some(version) = suffix
        .strip_prefix('/')
        .and_then(|suffix| suffix.split('/').next())
        && base.ends_with(&format!("/{version}"))
        && let Some(rest) = suffix
            .strip_prefix(&format!("/{version}"))
            .filter(|rest| rest.starts_with('/'))
    {
        url.set_path(&format!("{base}{rest}"));
        return Ok(url.to_string());
    }
    url.set_path(&format!("{base}{suffix}"));
    Ok(url.to_string())
}

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
    match capability {
        ModelCapability::Chat | ModelCapability::Vision | ModelCapability::WebSearch => {
            match protocol_type {
                "openai" => Ok(DispatchAction::JsonPost {
                    url: join_api_path(base_url, "/v1/chat/completions")?,
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
                        url: join_api_path(base_url, "/v1/messages")?,
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
                url: join_api_path(base_url, "/v1/embeddings")?,
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
                url: join_api_path(base_url, "/v1/audio/transcriptions")?,
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
                url: join_api_path(base_url, "/v1/audio/speech")?,
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

    #[test]
    fn join_api_path_appends_versioned_path_to_plain_base() {
        assert_eq!(
            join_api_path("https://api.moonshot.cn", "/v1/chat/completions").unwrap(),
            "https://api.moonshot.cn/v1/chat/completions"
        );
        assert_eq!(
            join_api_path("http://localhost:11434", "/v1/chat/completions").unwrap(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn join_api_path_rejects_embedded_credentials() {
        let error = join_api_path(
            "https://user:secret@provider.example/v1",
            "/v1/chat/completions",
        )
        .unwrap_err();

        assert!(matches!(
            error,
            GatewayError::InvalidInput(message)
                if message.contains("without credentials") && !message.contains("secret")
        ));
    }

    #[test]
    fn join_api_path_avoids_duplicated_version_prefix() {
        // base_url 已带 /v1（moonshot 官方文档的标准写法）→ 不能再拼出 /v1/v1/...
        assert_eq!(
            join_api_path("https://api.moonshot.cn/v1", "/v1/chat/completions").unwrap(),
            "https://api.moonshot.cn/v1/chat/completions"
        );
        // 尾斜杠同样处理
        assert_eq!(
            join_api_path("https://api.moonshot.cn/v1/", "/v1/chat/completions").unwrap(),
            "https://api.moonshot.cn/v1/chat/completions"
        );
        // anthropic 版本前缀
        assert_eq!(
            join_api_path("https://api.anthropic.com/v1", "/v1/messages").unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        // embedding / stt / tts 端点
        assert_eq!(
            join_api_path("https://api.openai.com/v1", "/v1/embeddings").unwrap(),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            join_api_path("https://api.openai.com/v1", "/v1/audio/speech").unwrap(),
            "https://api.openai.com/v1/audio/speech"
        );
    }

    #[test]
    fn join_api_path_keeps_full_endpoint_in_base_url() {
        assert_eq!(
            join_api_path(
                "https://api.moonshot.cn/v1/chat/completions",
                "/v1/chat/completions"
            )
            .unwrap(),
            "https://api.moonshot.cn/v1/chat/completions"
        );
        assert_eq!(
            join_api_path(
                "https://api.moonshot.cn/v1/chat/completions/",
                "/v1/chat/completions"
            )
            .unwrap(),
            "https://api.moonshot.cn/v1/chat/completions"
        );
    }

    #[test]
    fn full_endpoint_base_is_reused_as_an_api_root_for_other_capabilities() {
        // A channel may expose more than one capability. A copied chat
        // endpoint must still route Embedding/TTS to sibling endpoints rather
        // than appending below `chat/completions`.
        let embedding = dispatch_proxy(
            &ModelCapability::Embedding,
            "openai",
            "https://provider.example/v1/chat/completions",
            serde_json::json!({"model": "embed", "input": "hello"}),
        )
        .unwrap();
        let DispatchAction::JsonPost { url, .. } = embedding else {
            panic!("expected embedding JSON post");
        };
        assert_eq!(url, "https://provider.example/v1/embeddings");

        let tts = dispatch_proxy(
            &ModelCapability::Tts,
            "openai",
            "https://provider.example/proxy/v1/chat/completions",
            serde_json::json!({"model": "tts", "input": "hello", "voice": "alloy"}),
        )
        .unwrap();
        let DispatchAction::TtsBinary { url, .. } = tts else {
            panic!("expected TTS binary post");
        };
        assert_eq!(url, "https://provider.example/proxy/v1/audio/speech");
    }

    #[test]
    fn join_api_path_preserves_query_and_discards_fragment() {
        assert_eq!(
            join_api_path(
                "https://provider.example/v1?tenant=ains#local-only",
                "/v1/chat/completions"
            )
            .unwrap(),
            "https://provider.example/v1/chat/completions?tenant=ains"
        );
    }
}
