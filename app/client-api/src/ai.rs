//! AI 传输层（Phase 5.1）：`POST /api/ai/response` 统一 envelope 的类型化封装。
//!
//! - `Client::response` / `Client::response_stream`：统一入口（非流式 JSON / SSE 流式）
//! - `chat` / `chat_stream` / `embed` / `stt` / `tts`：类型化便捷方法
//!   （vision 即带 `input_image` content part 的 chat 消息，不设独立方法）
//! - 服务端契约见 `server/src/handlers/responses.rs`：chat/vision 走 Responses
//!   envelope 翻译代理；embedding/stt/tts 走直连路径；失败统一为
//!   `status="failed"` + `error{code,message}` 信封（`ai_response_error_body`）。

use std::collections::HashMap;

use base64::Engine as _;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::{Client, MAX_BACKOFF_SHIFT};
use crate::error::ClientError;

/// 统一 AI 能力入口路径。
pub const AI_RESPONSE_PATH: &str = "/api/ai/response";

/// SSE 事件缓冲上限：上游异常不发事件分隔符时防止内存无界增长。
const MAX_SSE_BUFFER_BYTES: usize = 4 * 1024 * 1024;

// ──────────────────────────────────────────────
//  请求 envelope
// ──────────────────────────────────────────────

/// `POST /api/ai/response` 请求 envelope（对齐服务端 `ResponsesRequest` +
/// AINS 路由扩展字段 `capability`）。
///
/// 直连能力（embedding/stt/tts）的专有字段（`audio`、`encoding_format` 等）
/// 通过 `extra` 平铺注入，由类型化便捷方法构造。
#[derive(Debug, Clone, Default, Serialize)]
pub struct AiRequest {
    /// AINS 路由字段："chat" | "vision" | "embedding" | "stt" | "tts" | "web_search"。
    /// 省略时由服务端按 input 内容自动检测。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// 目标模型；省略时由服务端按可用通道路由。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 模型输入：纯文本 / 文本数组（embedding）/ 消息数组（chat/vision）/ 音频（stt）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<AiInput>,
    /// 系统级指令（映射上游 system 消息）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// 开发者级指令（映射上游 developer 消息）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// 采样温度，服务端要求 0.0..=1.0。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// SSE 流式开关（仅 chat/vision 支持）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// 可选元数据（值必须全为字符串）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
    /// 直连能力专有字段（如 tts 的 `audio`、embedding 的 `encoding_format`），
    /// 序列化时平铺到 envelope 顶层。
    ///
    /// 注意：`extra` 的键不得与上方的具名字段（`capability`/`model`/`input`/
    /// `instructions`/`developer_instructions`/`max_output_tokens`/`temperature`/
    /// `stream`/`metadata`）重名，否则 `#[serde(flatten)]` 会产生重复键。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// 模型输入（对齐服务端 `Input` 的 untagged 序列化形态 + 直连能力扩展）。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum AiInput {
    /// 纯文本（chat 短形式 / tts 文本 / embedding 单条）。
    Text(String),
    /// 文本数组（embedding 批量）。
    Texts(Vec<String>),
    /// 消息数组（chat/vision 长形式，支持多模态 content part）。
    Messages(Vec<AiInputMessage>),
    /// 音频输入（stt 顶层 `input` 对象写法）。
    Audio(AiAudioInput),
}

/// 单条输入消息。
#[derive(Debug, Clone, Serialize)]
pub struct AiInputMessage {
    /// "user" | "assistant" | "system" | "developer"。
    pub role: String,
    pub content: AiContent,
}

impl AiInputMessage {
    /// 纯文本 user 消息。
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: AiContent::Text(text.into()),
        }
    }

    /// 纯文本 assistant 消息。
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: AiContent::Text(text.into()),
        }
    }

    /// 多模态 user 消息（vision：混合 `input_text` / `input_image` part）。
    pub fn user_parts(parts: Vec<AiContentPart>) -> Self {
        Self {
            role: "user".to_string(),
            content: AiContent::Parts(parts),
        }
    }
}

/// 消息内容：纯文本或多模态 part 数组（untagged）。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum AiContent {
    Text(String),
    Parts(Vec<AiContentPart>),
}

/// 多模态 content part（对齐服务端 `ContentPart` 的 tag="type" snake_case）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AiContentPart {
    InputText {
        text: String,
    },
    /// 图片：URL 或 `data:image/...;base64,...` data URI。
    InputImage {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// 音频（base64）：`format` 如 "wav"、"mp3"。
    InputAudio {
        data: String,
        format: String,
    },
}

/// STT 音频输入对象：`{"data": "<base64>", "format": "wav"}`。
#[derive(Debug, Clone, Serialize)]
pub struct AiAudioInput {
    pub data: String,
    pub format: String,
}

// ──────────────────────────────────────────────
//  响应 envelope
// ──────────────────────────────────────────────

/// 统一响应信封（成功与失败共用同一 schema，失败时 `status="failed"` +
/// `error` 非空；`output` 项的形态随 capability 变化，以 `Value` 承载并
/// 提供类型化提取方法）。
#[derive(Debug, Clone, Deserialize)]
pub struct AiResponse {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub capability: Option<String>,
    /// "completed" | "incomplete" | "failed" | "in_progress"（流式 created 事件）。
    pub status: String,
    #[serde(default)]
    pub incomplete_details: Option<Value>,
    #[serde(default)]
    pub output: Vec<Value>,
    #[serde(default)]
    pub usage: Option<AiUsage>,
    #[serde(default)]
    pub error: Option<AiErrorBody>,
}

/// token 用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct AiUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// 失败信封中的错误体。
#[derive(Debug, Clone, Deserialize)]
pub struct AiErrorBody {
    pub code: String,
    pub message: String,
}

impl AiResponse {
    /// 拼接全部 `message` 输出项中的 `output_text` 文本（chat/vision）。
    pub fn output_text(&self) -> String {
        let mut text = String::new();
        for item in &self.output {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let Some(parts) = item.get("content").and_then(Value::as_array) else {
                continue;
            };
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("output_text")
                    && let Some(t) = part.get("text").and_then(Value::as_str)
                {
                    text.push_str(t);
                }
            }
        }
        text
    }

    /// 首个安全拒答文本（chat/vision 的 `refusal` content part）。
    pub fn refusal(&self) -> Option<String> {
        for item in &self.output {
            let Some(parts) = item.get("content").and_then(Value::as_array) else {
                continue;
            };
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("refusal")
                    && let Some(r) = part.get("refusal").and_then(Value::as_str)
                {
                    return Some(r.to_string());
                }
            }
        }
        None
    }

    /// STT 转写文本（`{"type":"transcription","text":...}` 输出项）。
    pub fn transcription(&self) -> Option<String> {
        self.output.iter().find_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some("transcription"))
                .then(|| item.get("text").and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        })
    }

    /// TTS 音频（`{"type":"audio","data":"<base64>","content_type":...}` 输出项）：
    /// 返回解码后的音频字节与 content type。
    pub fn audio(&self) -> Result<Option<AiAudioOutput>, ClientError> {
        for item in &self.output {
            if item.get("type").and_then(Value::as_str) != Some("audio") {
                continue;
            }
            let data = item
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| ClientError::Deserialization("audio output missing data".into()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| {
                    ClientError::Deserialization(format!("invalid base64 audio data: {e}"))
                })?;
            let content_type = item
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Ok(Some(AiAudioOutput {
                data: bytes,
                content_type,
            }));
        }
        Ok(None)
    }

    /// embedding 向量组：按输出项的 `index` 升序返回（缺失 index 时回退到
    /// 出现顺序），使返回顺序与输入文本一一对应，不依赖服务端数组顺序。
    ///
    /// 非法 index、不一致维度或超出有限 `f32` 范围的 JSON 数字会返回
    /// [`ClientError::Deserialization`]，避免将协议错误静默转换为“无向量”。
    pub fn embeddings(&self) -> Result<Vec<Vec<f32>>, ClientError> {
        if self.output.is_empty() {
            return Err(ClientError::Deserialization(
                "embedding response contained no output items".into(),
            ));
        }

        let mut indexed = Vec::new();
        let mut expected_dimensions = None;
        for (pos, item) in self.output.iter().enumerate() {
            let values = item
                .get("embedding")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    ClientError::Deserialization(
                        "embedding output item must contain an embedding array".into(),
                    )
                })?;
            if values.is_empty() {
                return Err(ClientError::Deserialization(
                    "embedding output must not be empty".into(),
                ));
            }

            let mut vector = Vec::with_capacity(values.len());
            for value in values {
                let value = value.as_f64().ok_or_else(|| {
                    ClientError::Deserialization(
                        "embedding output contains a non-numeric value".into(),
                    )
                })? as f32;
                if !value.is_finite() {
                    return Err(ClientError::Deserialization(
                        "embedding output contains a value outside the finite f32 range".into(),
                    ));
                }
                vector.push(value);
            }

            match expected_dimensions {
                Some(expected) if vector.len() != expected => {
                    return Err(ClientError::Deserialization(
                        "embedding output vectors must all have the same dimensions".into(),
                    ));
                }
                None => expected_dimensions = Some(vector.len()),
                Some(_) => {}
            }

            let index = match item.get("index") {
                None => pos as u64,
                Some(index) => index.as_u64().ok_or_else(|| {
                    ClientError::Deserialization(
                        "embedding output index must be a non-negative integer".into(),
                    )
                })?,
            };
            indexed.push((index, vector));
        }
        indexed.sort_by_key(|(index, _)| *index);
        if indexed
            .iter()
            .enumerate()
            .any(|(expected, (actual, _))| *actual != expected as u64)
        {
            return Err(ClientError::Deserialization(
                "embedding output indices must be unique and contiguous from zero".into(),
            ));
        }
        Ok(indexed.into_iter().map(|(_, vector)| vector).collect())
    }
}

/// TTS 音频输出（已解码字节）。
#[derive(Debug, Clone)]
pub struct AiAudioOutput {
    pub data: Vec<u8>,
    pub content_type: String,
}

// ──────────────────────────────────────────────
//  SSE 流式事件
// ──────────────────────────────────────────────

/// chat/vision SSE 流式事件（对齐服务端 `sse_events` 下发的事件序列）。
#[derive(Debug, Clone)]
pub enum AiStreamEvent {
    /// `response.created`：流开始（response.status = "in_progress"）。
    Created,
    /// `response.output_text.delta`：文本增量。
    OutputTextDelta {
        delta: String,
    },
    /// `response.refusal.delta`：安全拒答增量。
    RefusalDelta {
        delta: String,
    },
    /// `response.output_text.done`：完整文本。
    OutputTextDone {
        text: String,
    },
    /// `response.refusal.done`：完整拒答。
    RefusalDone {
        refusal: String,
    },
    /// 终止事件三选一：`response.completed` / `response.incomplete` /
    /// `response.failed`，携带完整响应载荷。
    Completed {
        response: AiResponse,
    },
    Incomplete {
        response: AiResponse,
    },
    Failed {
        response: AiResponse,
    },
    /// 流内 `error` 事件（发出后服务端关闭连接，不再有终止事件）。
    Error {
        code: String,
        message: String,
    },
    /// 结构性事件（output_item/content_part added/done 等），透传原始数据。
    Other {
        event: String,
        data: Value,
    },
}

/// SSE 事件流：Native 端要求 `Send`，WASM 端为本地流（与 rust-agent
/// `EventStream` 同一 cfg 收敛策略）。
#[cfg(not(target_arch = "wasm32"))]
pub type AiEventStream = futures::stream::BoxStream<'static, Result<AiStreamEvent, ClientError>>;
#[cfg(target_arch = "wasm32")]
pub type AiEventStream =
    futures::stream::LocalBoxStream<'static, Result<AiStreamEvent, ClientError>>;

// ──────────────────────────────────────────────
//  Client 扩展：统一入口 + 类型化便捷方法
// ──────────────────────────────────────────────

impl Client {
    /// 统一非流式入口 — `POST /api/ai/response`。
    ///
    /// 失败信封（`status="failed"`）解析为 [`ClientError::Api`]；非信封错误
    /// （如中间件 429）沿用既有错误映射。复用 `send_and_parse` 的重试管线
    /// （网络错误 / 5xx / 429 指数退避）。
    pub async fn response(&self, request: &AiRequest) -> Result<AiResponse, ClientError> {
        let url = self.config().build_url(AI_RESPONSE_PATH);
        let builder = self.request_with_auth(reqwest::Method::POST, &url, None)?;
        let result: Result<AiResponse, ClientError> =
            self.send_and_parse(builder.json(request)).await;
        match result {
            Ok(response) => {
                // 防御：2xx 却带 failed 状态（协议上不应出现）也归一为 Api 错误。
                // 200 为合成占位状态码：send_and_parse 不透出真实 2xx 码，
                // 此处仅表达“传输成功但业务失败”，诊断以 error.code 为准
                if response.status == "failed" {
                    return Err(ai_error_from_response(200, &response));
                }
                Ok(response)
            }
            Err(err) => Err(map_ai_error(err)),
        }
    }

    /// 统一 SSE 流式入口 — `POST /api/ai/response`（`stream=true`）。
    ///
    /// 仅在建立连接阶段按 `max_retries` 重试（网络错误 / 5xx / 429）；
    /// 流一旦建立，中途失败以 [`AiStreamEvent::Error`] / 流内 `Err` 上报，
    /// 不在传输层重试（重试语义由上层 ModelClient 决策，见 Phase 5.2）。
    pub async fn response_stream(&self, request: &AiRequest) -> Result<AiEventStream, ClientError> {
        let mut request = request.clone();
        request.stream = Some(true);
        let url = self.config().build_url(AI_RESPONSE_PATH);
        let max_retries = self.config().max_retries;

        let mut last_err: Option<ClientError> = None;
        for attempt in 0..=max_retries {
            if attempt > 0 {
                // 与 send_and_parse 相同的退避序列：500ms, 1s, 2s（上限）
                let delay_ms = 500u64 * (1u64 << (attempt - 1).min(MAX_BACKOFF_SHIFT));
                Self::sleep_ms(delay_ms as u32).await;
            }
            let builder = self.request_with_auth(reqwest::Method::POST, &url, None)?;
            let response = match builder.json(&request).send().await {
                Ok(response) => response,
                Err(e) => {
                    let err: ClientError = e.into();
                    if should_retry_stream(&err) && attempt < max_retries {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                let err = map_ai_error(ClientError::from_status(
                    status.as_u16(),
                    truncate_error_body(body),
                ));
                if should_retry_stream(&err) && attempt < max_retries {
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if !content_type.contains("text/event-stream") {
                // 2xx 但非 SSE：按失败信封解析（服务端流开始前失败走 JSON）。
                // 非信封 body 截断后才入错误串，避免任意大响应体经
                // Display 全量外泄（失败信封远小于上限，解析不受影响）
                let body = response.text().await.unwrap_or_default();
                return Err(map_ai_error(ClientError::from_status(
                    status.as_u16(),
                    truncate_error_body(body),
                )));
            }
            return Ok(sse_event_stream(response));
        }
        Err(last_err
            .unwrap_or_else(|| ClientError::Network("Stream request retries exhausted".into())))
    }

    /// chat 便捷方法（非流式）。vision 即消息中带 `input_image` part。
    pub async fn chat(
        &self,
        messages: Vec<AiInputMessage>,
        options: &ChatOptions,
    ) -> Result<AiResponse, ClientError> {
        self.response(&options.to_request(messages)).await
    }

    /// chat 便捷方法（SSE 流式）。
    pub async fn chat_stream(
        &self,
        messages: Vec<AiInputMessage>,
        options: &ChatOptions,
    ) -> Result<AiEventStream, ClientError> {
        self.response_stream(&options.to_request(messages)).await
    }

    /// embedding 便捷方法：批量文本 → 向量组（顺序与输入一致）。
    pub async fn embed(
        &self,
        texts: Vec<String>,
        model: Option<&str>,
    ) -> Result<Vec<Vec<f32>>, ClientError> {
        let expected_vectors = texts.len();
        let request = AiRequest {
            capability: Some("embedding".to_string()),
            model: model.map(str::to_string),
            input: Some(AiInput::Texts(texts)),
            ..Default::default()
        };
        let response = self.response(&request).await?;
        let vectors = response.embeddings()?;
        if vectors.len() != expected_vectors {
            return Err(ClientError::Deserialization(format!(
                "embedding output count mismatch: expected {expected_vectors}, received {}",
                vectors.len()
            )));
        }
        Ok(vectors)
    }

    /// STT 便捷方法：音频字节（客户端负责 base64 编码）→ 转写文本。
    ///
    /// `format` 为服务端支持的音频格式之一：
    /// flac | mp3 | mp4 | mpeg | mpga | m4a | ogg | wav | webm。
    pub async fn stt(
        &self,
        audio: &[u8],
        format: &str,
        model: Option<&str>,
    ) -> Result<String, ClientError> {
        let request = AiRequest {
            capability: Some("stt".to_string()),
            model: model.map(str::to_string),
            input: Some(AiInput::Audio(AiAudioInput {
                data: base64::engine::general_purpose::STANDARD.encode(audio),
                format: format.to_string(),
            })),
            ..Default::default()
        };
        let response = self.response(&request).await?;
        response.transcription().ok_or_else(|| {
            ClientError::Deserialization("STT response missing transcription output".into())
        })
    }

    /// TTS 便捷方法：文本 → 解码后的音频字节 + content type。
    pub async fn tts(
        &self,
        text: &str,
        voice: &str,
        options: &TtsOptions,
    ) -> Result<AiAudioOutput, ClientError> {
        let mut audio = serde_json::Map::new();
        audio.insert("voice".into(), Value::String(voice.to_string()));
        if let Some(format) = &options.format {
            audio.insert("format".into(), Value::String(format.clone()));
        }
        if let Some(speed) = options.speed {
            audio.insert(
                "speed".into(),
                serde_json::Number::from_f64(speed)
                    .map(Value::Number)
                    .ok_or_else(|| ClientError::Config("TTS speed must be finite".into()))?,
            );
        }
        let mut extra = serde_json::Map::new();
        extra.insert("audio".into(), Value::Object(audio));
        let request = AiRequest {
            capability: Some("tts".to_string()),
            model: options.model.clone(),
            input: Some(AiInput::Text(text.to_string())),
            extra,
            ..Default::default()
        };
        let response = self.response(&request).await?;
        response
            .audio()?
            .ok_or_else(|| ClientError::Deserialization("TTS response missing audio output".into()))
    }
}

/// chat/vision 请求选项。
#[derive(Debug, Clone, Default)]
pub struct ChatOptions {
    pub model: Option<String>,
    /// 系统级指令（system prompt）。
    pub instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
}

impl ChatOptions {
    fn to_request(&self, messages: Vec<AiInputMessage>) -> AiRequest {
        AiRequest {
            model: self.model.clone(),
            input: Some(AiInput::Messages(messages)),
            instructions: self.instructions.clone(),
            developer_instructions: self.developer_instructions.clone(),
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            ..Default::default()
        }
    }
}

/// TTS 请求选项。
#[derive(Debug, Clone, Default)]
pub struct TtsOptions {
    pub model: Option<String>,
    /// mp3 | opus | aac | flac | wav | pcm。
    pub format: Option<String>,
    /// 0.25..=4.0。
    pub speed: Option<f64>,
}

// ──────────────────────────────────────────────
//  错误映射
// ──────────────────────────────────────────────

/// 入错误串的响应体上限（失败信封 JSON 远小于此值，截断不影响
/// `map_ai_error` 对合法信封的解析）。
const MAX_ERROR_BODY_BYTES: usize = 2048;

/// 按字符边界截断超长响应体，防止任意大 body 嵌入错误串。
fn truncate_error_body(mut body: String) -> String {
    if body.len() <= MAX_ERROR_BODY_BYTES {
        return body;
    }
    let mut cut = MAX_ERROR_BODY_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    body.truncate(cut);
    body.push_str("...[truncated]");
    body
}

/// 将非 2xx 响应体尝试解析为失败信封；无法解析时保留原错误。
fn map_ai_error(err: ClientError) -> ClientError {
    let (status, body) = match &err {
        ClientError::ServerError(status, body) | ClientError::Other(status, body) => {
            (*status, body.as_str())
        }
        ClientError::RateLimited(body) => (429, body.as_str()),
        _ => return err,
    };
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return err;
    };
    // 失败信封判定：object=response + status=failed + error{code,message}
    if value.get("object").and_then(Value::as_str) != Some("response") {
        return err;
    }
    let Some(error) = value.get("error").filter(|e| !e.is_null()) else {
        return err;
    };
    ClientError::Api {
        status,
        code: error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// 2xx 却带 failed 信封时的归一化（防御路径）。
fn ai_error_from_response(status: u16, response: &AiResponse) -> ClientError {
    match &response.error {
        Some(error) => ClientError::Api {
            status,
            code: error.code.clone(),
            message: error.message.clone(),
        },
        None => ClientError::Api {
            status,
            code: "unknown".to_string(),
            message: "AI response reported failed status without error body".to_string(),
        },
    }
}

/// 流式建立阶段的可重试判定（与 `send_and_parse` 同口径）。
fn should_retry_stream(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::Network(_) | ClientError::ServerError(..) | ClientError::RateLimited(_)
    ) || matches!(err, ClientError::Api { status, .. } if *status == 429 || *status >= 500)
}

// ──────────────────────────────────────────────
//  SSE 解析
// ──────────────────────────────────────────────

/// 将 SSE 字节流包装为事件流。
fn sse_event_stream(response: reqwest::Response) -> AiEventStream {
    let state = SseState {
        bytes: Box::pin(response.bytes_stream()),
        buffer: Vec::new(),
        scan_from: 0,
        done: false,
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if state.done {
                return None;
            }
            // 先消费缓冲区中已完整的事件块
            while let Some(block) = take_next_sse_event(&mut state.buffer, &mut state.scan_from) {
                let block = match block {
                    Ok(block) => block,
                    Err(_) => {
                        state.done = true;
                        return Some((
                            Err(ClientError::Deserialization(
                                "SSE event is not valid UTF-8".into(),
                            )),
                            state,
                        ));
                    }
                };
                match parse_sse_block(&block) {
                    Ok(Some(event)) => {
                        // 终止事件与流内 error 之后不再有数据
                        if matches!(
                            event,
                            AiStreamEvent::Completed { .. }
                                | AiStreamEvent::Incomplete { .. }
                                | AiStreamEvent::Failed { .. }
                                | AiStreamEvent::Error { .. }
                        ) {
                            state.done = true;
                        }
                        return Some((Ok(event), state));
                    }
                    Ok(None) => continue, // keepalive 注释等
                    // 终止事件载荷非法：显式上报而非静默丢弃，避免消费方
                    // 无法区分“连接断开”与“终止事件损坏”
                    Err(err) => {
                        state.done = true;
                        return Some((Err(err), state));
                    }
                }
            }
            match state.bytes.next().await {
                Some(Ok(chunk)) => {
                    if state.buffer.len().saturating_add(chunk.len()) > MAX_SSE_BUFFER_BYTES {
                        state.done = true;
                        return Some((
                            Err(ClientError::Deserialization(
                                "SSE buffer overflow: event exceeds 4 MiB".into(),
                            )),
                            state,
                        ));
                    }
                    state.buffer.extend_from_slice(&chunk);
                }
                Some(Err(e)) => {
                    state.done = true;
                    return Some((Err(ClientError::from(e)), state));
                }
                None => return None, // 连接关闭（无终止事件：由上层判定为异常结束）
            }
        }
    }))
}

/// SSE 解析中间状态。
struct SseState {
    #[cfg(not(target_arch = "wasm32"))]
    bytes: futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    #[cfg(target_arch = "wasm32")]
    bytes: futures::stream::LocalBoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    buffer: Vec<u8>,
    /// 分隔符扫描的起始偏移：已扫过无分隔符的前缀不重扫（避免单个
    /// 大事件分小 chunk 到达时退化 O(n²)）。
    scan_from: usize,
    done: bool,
}

/// 取出缓冲区中下一个完整事件块（分隔符：两个连续行终止符，行终止符
/// 按 SSE 规范为 `\r\n` / `\n` / `\r`，允许混用）。
///
/// `scan_from` 为跨调用的扫描起点：未找到分隔符时回退到 `len-3`（未决
/// 分隔符最长前缀为末尾 `\r\n\r`，起点必 ≥ len-3）；切出事件后归零。
fn take_next_sse_event(
    buffer: &mut Vec<u8>,
    scan_from: &mut usize,
) -> Option<Result<String, std::string::FromUtf8Error>> {
    let Some((position, delimiter_len)) = find_sse_delimiter(buffer, *scan_from) else {
        *scan_from = buffer.len().saturating_sub(3);
        return None;
    };
    *scan_from = 0;
    let remaining = buffer.split_off(position + delimiter_len);
    let mut event = std::mem::replace(buffer, remaining);
    event.truncate(position);
    Some(String::from_utf8(event))
}

/// `pos` 处的行终止符长度（`\r\n`=2 / `\n`=1 / `\r`=1）。缓冲区末尾的
/// 孤立 `\r` 存在跨 chunk 歧义（可能是 `\r\n` 前半），返回 None 等待
/// 更多数据。
fn line_terminator_len(buffer: &[u8], pos: usize) -> Option<usize> {
    match buffer.get(pos)? {
        b'\n' => Some(1),
        b'\r' => match buffer.get(pos + 1) {
            Some(b'\n') => Some(2),
            Some(_) => Some(1),
            None => None, // 尾部孤立 \r：等下一个 chunk 消歧义
        },
        _ => None,
    }
}

fn find_sse_delimiter(buffer: &[u8], start: usize) -> Option<(usize, usize)> {
    for position in start..buffer.len() {
        if let Some(first) = line_terminator_len(buffer, position)
            && let Some(second) = line_terminator_len(buffer, position + first)
        {
            return Some((position, first + second));
        }
    }
    None
}

/// 解析单个事件块：提取 `event:` 名称与合并多行 `data:` 载荷（SSE 规范：
/// 多行 data 以换行连接、冒号后单个空格剥离、注释行忽略）。块内行
/// 切分支持 `\r\n` / `\n` / `\r` 三种行终止符，与块切分层
/// `line_terminator_len` 同口径（`str::lines` 不识别孤立 `\r`，若中间层
/// 规范化行尾为 `\r`，终止事件会被静默丢弃，绕过 CR-9 的错误上报）。
///
/// 返回语义：`Ok(None)` 为可忽略块（注释/无事件名/无载荷，含非终止事件
/// 的非法 JSON，丢失内容由终止事件全量文本兑付）；终止事件
/// （completed/incomplete/failed）载荷非法时返回 `Err`，使消费方能区分
/// “连接断开”与“终止事件损坏”（否则表现为无诊断的异常流结束）。
fn parse_sse_block(block: &str) -> Result<Option<AiStreamEvent>, ClientError> {
    let mut event_name: Option<&str> = None;
    let mut data = String::new();
    let mut has_data = false;
    // `split(['\n','\r'])` 对 `\r\n` 产生一个空片段，无前缀匹配自然忽略
    for line in block.split(['\n', '\r']) {
        if let Some(name) = line.strip_prefix("event:") {
            event_name = Some(name.trim());
        } else if let Some(value) = line.strip_prefix("data:") {
            if has_data {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            has_data = true;
        }
        // 其余行（`:` 注释 / id / retry）忽略
    }
    let Some(event_name) = event_name else {
        return Ok(None);
    };
    if !has_data {
        return Ok(None);
    }
    let is_terminal = matches!(
        event_name,
        "response.completed" | "response.incomplete" | "response.failed"
    );
    let value: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(err) => {
            if is_terminal {
                return Err(ClientError::Deserialization(format!(
                    "malformed terminal SSE event `{event_name}`: {err}"
                )));
            }
            return Ok(None);
        }
    };
    Ok(Some(match event_name {
        "response.created" => AiStreamEvent::Created,
        "response.output_text.delta" => AiStreamEvent::OutputTextDelta {
            delta: value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "response.refusal.delta" => AiStreamEvent::RefusalDelta {
            delta: value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "response.output_text.done" => AiStreamEvent::OutputTextDone {
            text: value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "response.refusal.done" => AiStreamEvent::RefusalDone {
            refusal: value
                .get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "response.completed" | "response.incomplete" | "response.failed" => {
            let payload = value.get("response").cloned().ok_or_else(|| {
                ClientError::Deserialization(format!(
                    "terminal SSE event `{event_name}` missing `response` payload"
                ))
            })?;
            let response: AiResponse = serde_json::from_value(payload).map_err(|err| {
                ClientError::Deserialization(format!(
                    "terminal SSE event `{event_name}` payload invalid: {err}"
                ))
            })?;
            match event_name {
                "response.completed" => AiStreamEvent::Completed { response },
                "response.incomplete" => AiStreamEvent::Incomplete { response },
                _ => AiStreamEvent::Failed { response },
            }
        }
        "error" => AiStreamEvent::Error {
            code: value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        other => AiStreamEvent::Other {
            event: other.to_string(),
            data: value,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ai_request_serializes_capability_and_extra_flattened() {
        let mut extra = serde_json::Map::new();
        extra.insert("audio".into(), json!({"voice": "alloy"}));
        let request = AiRequest {
            capability: Some("tts".into()),
            input: Some(AiInput::Text("hello".into())),
            extra,
            ..Default::default()
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["capability"], "tts");
        assert_eq!(value["input"], "hello");
        assert_eq!(value["audio"]["voice"], "alloy");
        // None 字段不应出现
        assert!(value.get("model").is_none());
        assert!(value.get("stream").is_none());
    }

    #[test]
    fn ai_input_messages_serialize_as_responses_shape() {
        let messages = vec![AiInputMessage::user_parts(vec![
            AiContentPart::InputText {
                text: "看看这张图".into(),
            },
            AiContentPart::InputImage {
                image_url: "data:image/png;base64,AAAA".into(),
                detail: None,
            },
        ])];
        let value = serde_json::to_value(AiInput::Messages(messages)).unwrap();
        assert_eq!(value[0]["role"], "user");
        assert_eq!(value[0]["content"][0]["type"], "input_text");
        assert_eq!(value[0]["content"][1]["type"], "input_image");
        assert!(value[0]["content"][1].get("detail").is_none());
    }

    #[test]
    fn take_next_sse_event_handles_all_delimiters_and_partials() {
        let mut scan = 0usize;
        let mut buffer = b"event: a\ndata: {}\n\nevent: b\r\n".to_vec();
        assert_eq!(
            take_next_sse_event(&mut buffer, &mut scan)
                .unwrap()
                .unwrap(),
            "event: a\ndata: {}"
        );
        // 剩余为不完整事件
        assert!(take_next_sse_event(&mut buffer, &mut scan).is_none());
        buffer.extend_from_slice(b"data: {\"x\":1}\r\n\r\n");
        assert_eq!(
            take_next_sse_event(&mut buffer, &mut scan)
                .unwrap()
                .unwrap(),
            "event: b\r\ndata: {\"x\":1}"
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn take_next_sse_event_handles_mixed_line_endings() {
        // 混合换行：行以 \n 结束、空行以 \r\n 结束（归一化代理可能产生）
        let mut scan = 0usize;
        let mut buffer = b"data: {}\n\r\nrest".to_vec();
        assert_eq!(
            take_next_sse_event(&mut buffer, &mut scan)
                .unwrap()
                .unwrap(),
            "data: {}"
        );
        assert_eq!(buffer, b"rest");

        // 尾部孤立 \r 是跨 chunk \r\n 的歧义前半：先等待，补全后切分
        let mut scan = 0usize;
        let mut buffer = b"data: {}\n\r".to_vec();
        assert!(take_next_sse_event(&mut buffer, &mut scan).is_none());
        buffer.push(b'\n');
        assert_eq!(
            take_next_sse_event(&mut buffer, &mut scan)
                .unwrap()
                .unwrap(),
            "data: {}"
        );
        assert!(buffer.is_empty());

        // \r\r 旧式 Mac 分隔符仍支持（非末尾位置无歧义）
        let mut scan = 0usize;
        let mut buffer = b"data: {}\r\rx".to_vec();
        assert_eq!(
            take_next_sse_event(&mut buffer, &mut scan)
                .unwrap()
                .unwrap(),
            "data: {}"
        );
        assert_eq!(buffer, b"x");

        // 尾部单个 \n 不构成分隔符（需第二个行终止符）：跨 chunk 到达后才产出
        let mut scan = 0usize;
        let mut buffer = b"data: x\n".to_vec();
        assert!(take_next_sse_event(&mut buffer, &mut scan).is_none());
        buffer.push(b'\n');
        assert_eq!(
            take_next_sse_event(&mut buffer, &mut scan)
                .unwrap()
                .unwrap(),
            "data: x"
        );
    }

    #[test]
    fn take_next_sse_event_byte_split_torture() {
        // 回归（scan_from 增量扫描）：固定事件序列在每个字节偏移处切分为
        // 两个 chunk（含多字节 UTF-8 字符中间、\r\n\r\n 分隔符内部），
        // 产出的事件块序列必须与一次性输入完全一致。
        let payload: &[u8] =
            b"event: a\ndata: {\"t\":\"\xe4\xbd\xa0\xe5\xa5\xbd\"}\r\n\r\nevent: b\rdata: {}\r\n\r\n";
        // 基准：一次性输入全部字节
        let mut expected = Vec::new();
        {
            let mut scan = 0usize;
            let mut buffer = payload.to_vec();
            while let Some(block) = take_next_sse_event(&mut buffer, &mut scan) {
                expected.push(block.unwrap());
            }
            assert_eq!(expected.len(), 2, "baseline must yield both events");
        }
        for split_at in 0..=payload.len() {
            let mut scan = 0usize;
            let mut buffer = Vec::new();
            let mut produced = Vec::new();
            for chunk in [&payload[..split_at], &payload[split_at..]] {
                buffer.extend_from_slice(chunk);
                while let Some(block) = take_next_sse_event(&mut buffer, &mut scan) {
                    produced.push(block.unwrap());
                }
            }
            assert_eq!(produced, expected, "split at byte {split_at} diverged");
            assert!(buffer.is_empty(), "split at byte {split_at} left residue");
        }
    }

    #[test]
    fn find_sse_delimiter_scan_offset_resumes_without_missing() {
        // 未找到分隔符时 scan_from 回退到 len-3：逐字节送入大事件也不丢分隔符
        let event = format!("data: {}\n\n", "x".repeat(500));
        let mut scan = 0usize;
        let mut buffer = Vec::new();
        let mut produced = Vec::new();
        for byte in event.as_bytes() {
            buffer.push(*byte);
            if let Some(block) = take_next_sse_event(&mut buffer, &mut scan) {
                produced.push(block.unwrap());
            }
            // 扫描起点永不越界，且落后缓冲区末尾不超过 3 字节
            assert!(scan <= buffer.len());
            assert!(buffer.len().saturating_sub(scan) <= 3);
        }
        assert_eq!(produced.len(), 1);
        assert!(produced[0].ends_with('x'));
        assert!(buffer.is_empty());
        assert_eq!(scan, 0, "scan offset must reset after extracting the event");
    }

    #[test]
    fn parse_sse_block_maps_known_events() {
        let delta = parse_sse_block(
            "event: response.output_text.delta\ndata: {\"delta\":\"你好\",\"sequence_number\":3}",
        )
        .unwrap()
        .unwrap();
        assert!(matches!(delta, AiStreamEvent::OutputTextDelta { delta } if delta == "你好"));

        let error = parse_sse_block(
            "event: error\ndata: {\"code\":\"provider_stream_error\",\"message\":\"boom\"}",
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(error, AiStreamEvent::Error { code, .. } if code == "provider_stream_error")
        );

        // keepalive 注释块无 data，跳过
        assert!(parse_sse_block(": keepalive").unwrap().is_none());
    }

    #[test]
    fn parse_sse_block_joins_multi_data_lines() {
        let event =
            parse_sse_block("event: response.output_text.done\ndata: {\"text\":\ndata: \"hi\"}")
                .unwrap();
        // 多行 data 以换行连接后仍是合法 JSON
        assert!(matches!(
            event,
            Some(AiStreamEvent::OutputTextDone { text }) if text == "hi"
        ));
    }

    #[test]
    fn parse_sse_block_terminal_event_carries_response() {
        let payload = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1", "object": "response", "created_at": 1,
                "model": "m", "capability": "chat", "status": "completed",
                "incomplete_details": null,
                "output": [{"type": "message", "id": "msg_1", "status": "completed",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "done", "annotations": []}]}],
                "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3},
                "error": null
            },
            "sequence_number": 9
        });
        let block = format!("event: response.completed\ndata: {payload}");
        let event = parse_sse_block(&block).unwrap().unwrap();
        let AiStreamEvent::Completed { response } = event else {
            panic!("expected Completed");
        };
        assert_eq!(response.output_text(), "done");
        assert_eq!(response.usage.unwrap().total_tokens, 3);
    }

    #[test]
    fn parse_sse_block_malformed_terminal_event_surfaces_error() {
        // 终止事件载荷非法：必须上报 Deserialization 错误而非静默丢弃，
        // 否则消费方无法区分“连接断开”与“终止事件损坏”
        // 整体 JSON 非法
        let err = parse_sse_block("event: response.completed\ndata: {not json")
            .expect_err("malformed terminal JSON must error");
        assert!(matches!(err, ClientError::Deserialization(_)));
        // 缺 `response` 字段
        let err = parse_sse_block("event: response.failed\ndata: {\"sequence_number\":1}")
            .expect_err("missing response payload must error");
        assert!(matches!(err, ClientError::Deserialization(_)));
        // `response` 字段型不匹配（缺必填字段）
        let err =
            parse_sse_block("event: response.incomplete\ndata: {\"response\":{\"id\":\"r1\"}}")
                .expect_err("invalid response payload must error");
        assert!(matches!(err, ClientError::Deserialization(_)));
        // 非终止事件的非法 JSON 仍为可忽略块（内容由终止事件全量文本兑付）
        assert!(
            parse_sse_block("event: response.output_text.delta\ndata: {broken")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parse_sse_block_handles_lone_cr_line_terminators() {
        // 块切分层（line_terminator_len）支持孤立 \r，行解析层须同口径：
        // 否则 \r 框架的事件名与 data 粘连成单行被整块丢弃，终止事件
        // 静默消失会绕过 CR-9 建立的错误上报保证
        let delta = parse_sse_block("event: response.output_text.delta\rdata: {\"delta\":\"hi\"}")
            .unwrap()
            .unwrap();
        assert!(matches!(delta, AiStreamEvent::OutputTextDelta { delta } if delta == "hi"));
        // \r 框架 + 多行 data 合并仍成立
        let done =
            parse_sse_block("event: response.output_text.done\rdata: {\"text\":\rdata: \"hi\"}")
                .unwrap();
        assert!(matches!(
            done,
            Some(AiStreamEvent::OutputTextDone { text }) if text == "hi"
        ));
        // \r 框架的终止事件损坏时仍走 Err 路径（CR-9 保证不被绕过）
        let err = parse_sse_block("event: response.completed\rdata: {broken")
            .expect_err("cr-framed malformed terminal must error");
        assert!(matches!(err, ClientError::Deserialization(_)));
    }

    #[test]
    fn truncate_error_body_bounds_length_on_char_boundary() {
        // 短 body 原样透传（失败信封解析不受影响）
        assert_eq!(truncate_error_body("{}".into()), "{}");
        // 超长 body 截断带标记，且多字节字符不被撕裂
        let long = "汉".repeat(MAX_ERROR_BODY_BYTES); // 3 字节/字，必超限
        let truncated = truncate_error_body(long);
        assert!(truncated.ends_with("...[truncated]"));
        assert!(truncated.len() <= MAX_ERROR_BODY_BYTES + "...[truncated]".len());
        assert!(truncated.chars().all(|c| c == '汉' || c.is_ascii()));
    }

    #[test]
    fn ai_response_extractors_cover_direct_capabilities() {
        let response: AiResponse = serde_json::from_value(json!({
            "id": "resp_1", "object": "response", "created_at": 1,
            "model": "m", "capability": "embedding", "status": "completed",
            "incomplete_details": null,
            "output": [
                {"embedding": [0.1, 0.2], "index": 0, "dimensions": 2},
                {"embedding": [0.3, 0.4], "index": 1, "dimensions": 2}
            ],
            "usage": null, "error": null
        }))
        .unwrap();
        let embeddings = response.embeddings().unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), 2);

        let stt: AiResponse = serde_json::from_value(json!({
            "id": "resp_2", "object": "response", "created_at": 1,
            "model": null, "capability": "stt", "status": "completed",
            "incomplete_details": null,
            "output": [{"type": "transcription", "text": "hello"}],
            "usage": null, "error": null
        }))
        .unwrap();
        assert_eq!(stt.transcription().as_deref(), Some("hello"));

        let tts: AiResponse = serde_json::from_value(json!({
            "id": "resp_3", "object": "response", "created_at": 1,
            "model": null, "capability": "tts", "status": "completed",
            "incomplete_details": null,
            "output": [{"type": "audio", "data": "YWJj", "content_type": "audio/mpeg"}],
            "usage": null, "error": null
        }))
        .unwrap();
        let audio = tts.audio().unwrap().unwrap();
        assert_eq!(audio.data, b"abc");
        assert_eq!(audio.content_type, "audio/mpeg");
    }

    #[test]
    fn embeddings_returned_in_index_order_not_array_order() {
        // 服务端若以乱序数组返回，embeddings() 仍按 index 升序对齐输入
        let response: AiResponse = serde_json::from_value(json!({
            "id": "resp_e", "object": "response", "created_at": 1,
            "model": "m", "capability": "embedding", "status": "completed",
            "incomplete_details": null,
            "output": [
                {"embedding": [1.0, 1.0], "index": 1, "dimensions": 2},
                {"embedding": [0.0, 0.0], "index": 0, "dimensions": 2}
            ],
            "usage": null, "error": null
        }))
        .unwrap();
        let embeddings = response.embeddings().unwrap();
        assert_eq!(embeddings.len(), 2);
        // index 0 对应 [0,0] 应排在前，index 1 对应 [1,1] 在后
        assert_eq!(embeddings[0], vec![0.0f32, 0.0f32]);
        assert_eq!(embeddings[1], vec![1.0f32, 1.0f32]);
    }

    #[test]
    fn embedding_extraction_rejects_malformed_and_non_finite_f32_values() {
        for item in [
            json!({}),
            json!({"embedding": null}),
            json!({"embedding": "AACAPw=="}),
            json!({"embedding": []}),
            json!({"embedding": [null]}),
            json!({"embedding": ["1.0"]}),
            json!({"embedding": [1e100]}),
        ] {
            let response: AiResponse = serde_json::from_value(json!({
                "id": "resp_e", "object": "response", "created_at": 1,
                "model": "m", "capability": "embedding", "status": "completed",
                "incomplete_details": null,
                "output": [item],
                "usage": null, "error": null
            }))
            .unwrap();

            assert!(
                response.embeddings().is_err(),
                "invalid embedding should fail extraction: {item}"
            );
        }

        let empty: AiResponse = serde_json::from_value(json!({
            "id": "resp_e", "object": "response", "created_at": 1,
            "model": "m", "capability": "embedding", "status": "completed",
            "incomplete_details": null, "output": [], "usage": null, "error": null
        }))
        .unwrap();
        assert!(empty.embeddings().is_err());

        let duplicate_indices: AiResponse = serde_json::from_value(json!({
            "id": "resp_e", "object": "response", "created_at": 1,
            "model": "m", "capability": "embedding", "status": "completed",
            "incomplete_details": null,
            "output": [
                {"embedding": [0.1], "index": 0},
                {"embedding": [0.2], "index": 0}
            ],
            "usage": null, "error": null
        }))
        .unwrap();
        assert!(duplicate_indices.embeddings().is_err());

        for invalid_index in [json!(null), json!("0"), json!(-1), json!(0.5)] {
            let response: AiResponse = serde_json::from_value(json!({
                "id": "resp_e", "object": "response", "created_at": 1,
                "model": "m", "capability": "embedding", "status": "completed",
                "incomplete_details": null,
                "output": [{"embedding": [0.1], "index": invalid_index}],
                "usage": null, "error": null
            }))
            .unwrap();
            assert!(
                matches!(
                    response.embeddings(),
                    Err(ClientError::Deserialization(message)) if message.contains("index")
                ),
                "present but invalid index must not fall back to output order: {invalid_index}"
            );
        }

        let inconsistent_dimensions: AiResponse = serde_json::from_value(json!({
            "id": "resp_e", "object": "response", "created_at": 1,
            "model": "m", "capability": "embedding", "status": "completed",
            "incomplete_details": null,
            "output": [
                {"embedding": [0.1, 0.2], "index": 0},
                {"embedding": [0.3], "index": 1}
            ],
            "usage": null, "error": null
        }))
        .unwrap();
        assert!(matches!(
            inconsistent_dimensions.embeddings(),
            Err(ClientError::Deserialization(message)) if message.contains("dimensions")
        ));

        let missing_indices: AiResponse = serde_json::from_value(json!({
            "id": "resp_e", "object": "response", "created_at": 1,
            "model": "m", "capability": "embedding", "status": "completed",
            "incomplete_details": null,
            "output": [
                {"embedding": [0.1, 0.2]},
                {"embedding": [0.3, 0.4]}
            ],
            "usage": null, "error": null
        }))
        .unwrap();
        assert_eq!(missing_indices.embeddings().unwrap().len(), 2);
    }

    #[test]
    fn map_ai_error_parses_failed_envelope() {
        let body = json!({
            "id": "resp_x", "object": "response", "created_at": 1,
            "model": null, "capability": null, "status": "failed",
            "incomplete_details": null, "output": [], "usage": null,
            "error": {"code": "no_active_plan", "message": "No active plan"}
        })
        .to_string();
        let err = map_ai_error(ClientError::Other(403, body));
        assert!(
            matches!(err, ClientError::Api { status: 403, ref code, .. } if code == "no_active_plan")
        );

        // 非信封错误保持原样（中间件 429 是 {"error": "rate_limited"} 形态）
        let raw = ClientError::RateLimited("{\"error\":\"rate_limited\"}".into());
        assert!(matches!(map_ai_error(raw), ClientError::RateLimited(_)));
    }
}
