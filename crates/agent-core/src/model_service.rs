//! Gateway ModelClient（Phase 5.2）：`ModelClient` trait 的真实传输实现，
//! 底层复用 `client-api` 的 AI 传输层（`POST /api/ai/response`）。
//!
//! 对齐 OpenHarness `api/client.py` 的最小流式协议：
//! - 重试常量：`MAX_RETRIES=3`、`BASE_DELAY=1.0s`、`MAX_DELAY=30.0s`、
//!   可重试状态码 {429, 500, 502, 503, 529}；退避 = min(1.0 × 2^attempt, 30)
//!   + 均匀抖动（0..delay×0.25）。
//! - 重试作为流内 [`ModelStreamEvent::Retry`] 事件上报而非静默；重试后
//!   已发出的 delta 不重复（按原始字符计数去重，对齐清单"不复刻"第 1 条）。
//! - 认证类失败（401/403）不重试，直接终止。
//!
//! **对齐偏差（工具桥接）**：服务端 AI Gateway 拒绝 function tools
//! （`tools are not supported by this AI response endpoint`），因此工具下发
//! 采用提示词内协议：工具清单 + `<tool_use>` 标签协议注入 system prompt，
//! 从最终 assistant 文本解析回 `ToolUse` content block；历史中的
//! `ToolUse`/`ToolResult` block 以同一协议文本回渲染。UI delta 流经
//! 状态机过滤，抑制 `<tool_use>...</tool_use>` 协议片段外漏。

use std::sync::Arc;

use client_api::{
    AiContentPart, AiInput, AiInputMessage, AiRequest, AiResponse, AiStreamEvent, Client,
    ClientError,
};
use futures::StreamExt;
use serde_json::Value;

use crate::error::AgentError;
use crate::kernel::messages::{ContentBlock, ConversationMessage, Role};
use crate::memory::now_ms;
use crate::model_client::{
    EventStream, ModelClient, ModelRequest, ModelStreamEvent, UsageSnapshot,
};
use crate::runtime_adapter::RuntimeAdapter;

/// 最大重试次数（基线 `MAX_RETRIES = 3`，总尝试 4 次）。
pub const MAX_RETRIES: u32 = 3;
/// 退避基准（基线 `BASE_DELAY = 1.0` 秒）。
pub const BASE_DELAY_SECS: f32 = 1.0;
/// 退避上限（基线 `MAX_DELAY = 30.0` 秒）。
pub const MAX_DELAY_SECS: f32 = 30.0;
/// 可重试 HTTP 状态码（基线 `RETRYABLE_STATUS_CODES`）。
pub const RETRYABLE_STATUS_CODES: [u16; 5] = [429, 500, 502, 503, 529];

/// 工具调用协议的起止标签（提示词内协议，见模块级偏差说明）。
pub const TOOL_USE_OPEN: &str = "<tool_use";
pub const TOOL_USE_CLOSE: &str = "</tool_use>";

/// Gateway ModelClient：泛型于 RuntimeAdapter（退避 sleep 双端复用）。
pub struct GatewayModelClient<R: RuntimeAdapter> {
    client: Client,
    _runtime: std::marker::PhantomData<fn() -> R>,
}

impl<R: RuntimeAdapter> GatewayModelClient<R> {
    /// 宿主注入已完成认证配置的 `client_api::Client`（token 跨 clone 共享）。
    ///
    /// 重试分层约定：本层已拥有完整重试策略（流内 Retry 事件 + 退避），
    /// 宿主应以 `ClientConfig::with_max_retries(0)` 关闭 client-api 建连层
    /// 重试，避免双层叠加（最坏 4×(N+1) 次物理请求）。
    ///
    /// 后果：关闭后 `embed`/`stt`/`tts` 直连能力（走 `send_and_parse`，
    /// 无本层流式重试兜底）对瞬态 5xx/429 将不重试，失败直接上报
    /// 调用方（Memory/Perception 层自行决策是否重发）。
    pub fn new(client: Client) -> Self {
        Self {
            client,
            _runtime: std::marker::PhantomData,
        }
    }

    /// 便捷构造为共享单例（Kernel / Memory / Perception 共用）。
    pub fn shared(client: Client) -> Arc<Self> {
        Arc::new(Self::new(client))
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl<R: RuntimeAdapter + 'static> ModelClient for GatewayModelClient<R> {
    async fn stream_response(
        &self,
        request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        let client = self.client.clone();
        let ai_request = build_ai_request(&request);

        let stream = async_stream::stream! {
            // 重试后已发出的原始字符数（含协议标记）；用于 delta 去重
            let mut emitted_raw_chars: usize = 0;
            // 协议标签过滤器与去重计数器同生命周期（跨重试持续）：
            // 重试时去重跳过的字符正是 gate 已消费过的前缀，状态延续才能
            // 保证中断于 <tool_use> 块内部时不向 UI 泄漏协议片段。
            let mut gate = ToolTagFilter::new();
            for attempt in 0..=MAX_RETRIES {
                let mut upstream = match client.response_stream(&ai_request).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        if attempt >= MAX_RETRIES || !is_retryable(&error) {
                            // 建连阶段最终失败：以异常 delta 终止（无 Complete，
                            // Kernel 归一为 recoverable Error 事件）
                            yield retry_exhausted_event(&error);
                            return;
                        }
                        let delay = retry_delay(attempt);
                        yield ModelStreamEvent::Retry {
                            message: error.to_string(),
                            attempt: attempt + 1,
                            max_attempts: MAX_RETRIES + 1,
                            delay_secs: delay,
                        };
                        R::sleep(std::time::Duration::from_secs_f32(delay)).await;
                        continue;
                    }
                };

                let mut raw_seen: usize = 0;
                let mut failure: Option<String> = None;
                let mut finished = false;

                while let Some(event) = upstream.next().await {
                    match event {
                        Ok(AiStreamEvent::OutputTextDelta { delta })
                        | Ok(AiStreamEvent::RefusalDelta { delta }) => {
                            // 去重：仅发出超过既有已发计数的原始字符
                            let chars: Vec<char> = delta.chars().collect();
                            let start = emitted_raw_chars.saturating_sub(raw_seen);
                            raw_seen += chars.len();
                            if start < chars.len() {
                                let fresh: String = chars[start..].iter().collect();
                                emitted_raw_chars = emitted_raw_chars.max(raw_seen);
                                let visible = gate.push(&fresh);
                                if !visible.is_empty() {
                                    yield ModelStreamEvent::TextDelta { text: visible };
                                }
                            }
                        }
                        Ok(AiStreamEvent::Completed { response })
                        | Ok(AiStreamEvent::Incomplete { response }) => {
                            let trailing = gate.flush();
                            if !trailing.is_empty() {
                                yield ModelStreamEvent::TextDelta { text: trailing };
                            }
                            yield complete_event(&response);
                            finished = true;
                            break;
                        }
                        Ok(AiStreamEvent::Failed { response }) => {
                            let message = response
                                .error
                                .as_ref()
                                .map(|e| format!("[{}] {}", e.code, e.message))
                                .unwrap_or_else(|| "AI provider stream failed".into());
                            // 终态错误码（认证/请求形状/配额类）：重试不会改变
                            // 结果，直接终止，避免无谓的退避延迟
                            if response
                                .error
                                .as_ref()
                                .is_some_and(|e| is_non_retryable_stream_code(&e.code))
                            {
                                yield terminal_failure_event(format!("request failed: {message}"));
                                return;
                            }
                            failure = Some(message);
                            break;
                        }
                        Ok(AiStreamEvent::Error { code, message }) => {
                            if is_non_retryable_stream_code(&code) {
                                yield terminal_failure_event(format!("request failed: [{code}] {message}"));
                                return;
                            }
                            failure = Some(format!("[{code}] {message}"));
                            break;
                        }
                        Ok(_) => {} // Created / Done / 结构性事件
                        Err(error) => {
                            if !is_retryable(&error) {
                                yield retry_exhausted_event(&error);
                                return;
                            }
                            failure = Some(error.to_string());
                            break;
                        }
                    }
                }
                if finished {
                    return;
                }
                // 流中断（failure）或连接关闭却无终止事件：可重试
                let message = failure
                    .unwrap_or_else(|| "stream closed without a terminal event".into());
                if attempt >= MAX_RETRIES {
                    yield terminal_failure_event(format!("retries exhausted: {message}"));
                    return;
                }
                let delay = retry_delay(attempt);
                yield ModelStreamEvent::Retry {
                    message,
                    attempt: attempt + 1,
                    max_attempts: MAX_RETRIES + 1,
                    delay_secs: delay,
                };
                R::sleep(std::time::Duration::from_secs_f32(delay)).await;
            }
        };
        Ok(Box::pin(stream))
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AgentError> {
        let vectors = self
            .client
            .embed(vec![text.to_string()], None)
            .await
            .map_err(map_client_error)?;
        vectors
            .into_iter()
            .next()
            .ok_or_else(|| AgentError::Model("embedding response contained no vectors".into()))
    }

    async fn stt(&self, audio_data: &[u8]) -> Result<String, AgentError> {
        let format = detect_audio_format(audio_data);
        self.client
            .stt(audio_data, format, None)
            .await
            .map_err(map_client_error)
    }

    async fn tts(&self, text: &str) -> Result<Vec<u8>, AgentError> {
        let audio = self
            .client
            .tts(text, DEFAULT_TTS_VOICE, &Default::default())
            .await
            .map_err(map_client_error)?;
        Ok(audio.data)
    }
}

/// TTS 默认音色（客户端常量，见 AINS_PLAN 附录 C 配置边界）。
pub const DEFAULT_TTS_VOICE: &str = "alloy";

// ──────────────────────────────────────────────
//  请求构建：ModelRequest → AiRequest
// ──────────────────────────────────────────────

/// 组装 AiRequest：历史消息渲染 + 工具协议注入 system prompt。
pub fn build_ai_request(request: &ModelRequest) -> AiRequest {
    let instructions = match (&request.system_prompt, request.tools.is_empty()) {
        (Some(prompt), true) => Some(prompt.clone()),
        (Some(prompt), false) => Some(format!(
            "{prompt}\n\n{}",
            render_tool_protocol(&request.tools)
        )),
        (None, true) => None,
        (None, false) => Some(render_tool_protocol(&request.tools)),
    };
    AiRequest {
        model: request.model.clone(),
        input: Some(AiInput::Messages(render_messages(&request.messages))),
        instructions,
        max_output_tokens: Some(request.max_output_tokens),
        ..Default::default()
    }
}

/// 工具协议段：可用工具清单 + `<tool_use>` 调用协议说明。
fn render_tool_protocol(tools: &[crate::tools::ToolDef]) -> String {
    let mut section = String::from(
        "# Tool Call Protocol\n\
         To call a tool, output a block in EXACTLY this format (multiple blocks allowed,\n\
         IDs must be unique within one reply):\n\
         <tool_use id=\"call_1\" name=\"tool_name\">\n\
         {\"arg\": \"value\"}\n\
         </tool_use>\n\
         The block content must be the tool input as strict JSON. Tool results will be\n\
         returned in the next user message inside <tool_result id=\"...\"> blocks.\n\
         Do not mention these tags to the user or wrap them in code fences.\n\n\
         ## Available Tools\n",
    );
    for tool in tools {
        section.push_str(&format!(
            "- {}: {}\n  input schema: {}\n",
            tool.name, tool.description, tool.input_schema
        ));
    }
    section
}

/// 历史消息 → Responses 消息数组（ToolUse/ToolResult 回渲染为协议文本）。
fn render_messages(messages: &[ConversationMessage]) -> Vec<AiInputMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let mut parts: Vec<AiContentPart> = Vec::new();
            let mut text_acc = String::new();
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } => text_acc.push_str(text),
                    ContentBlock::Image { media_type, data } => {
                        if !text_acc.is_empty() {
                            parts.push(AiContentPart::InputText {
                                text: std::mem::take(&mut text_acc),
                            });
                        }
                        parts.push(AiContentPart::InputImage {
                            image_url: format!("data:{media_type};base64,{data}"),
                            detail: None,
                        });
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        text_acc.push_str(&format!(
                            "\n<tool_use id=\"{id}\" name=\"{name}\">\n{input}\n{TOOL_USE_CLOSE}\n"
                        ));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {
                        text_acc.push_str(&format!(
                            "\n<tool_result id=\"{tool_use_id}\" is_error=\"{is_error}\">\n\
                             {content}\n</tool_result>\n"
                        ));
                    }
                }
            }
            if parts.is_empty() {
                if text_acc.trim().is_empty() {
                    return None;
                }
                return Some(AiInputMessage {
                    role: role.to_string(),
                    content: client_api::AiContent::Text(text_acc),
                });
            }
            if !text_acc.is_empty() {
                parts.push(AiContentPart::InputText { text: text_acc });
            }
            Some(AiInputMessage {
                role: role.to_string(),
                content: client_api::AiContent::Parts(parts),
            })
        })
        .collect()
}

// ──────────────────────────────────────────────
//  响应转换：AiResponse → Complete 事件
// ──────────────────────────────────────────────

fn complete_event(response: &AiResponse) -> ModelStreamEvent {
    let raw_text = {
        let text = response.output_text();
        if text.is_empty() {
            response.refusal().unwrap_or_default()
        } else {
            text
        }
    };
    let message = ConversationMessage {
        role: Role::Assistant,
        content: parse_assistant_content(&raw_text),
    };
    let usage = response
        .usage
        .map(|u| UsageSnapshot {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        })
        .unwrap_or_default();
    let stop_reason = response
        .incomplete_details
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    ModelStreamEvent::Complete {
        message,
        usage,
        stop_reason,
    }
}

/// 从 assistant 全文解析协议块：文本段 + `ToolUse` block 交替还原。
/// 格式非法的块按原样保留为文本（fail-open，不丢内容）。
pub fn parse_assistant_content(text: &str) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut rest = text;
    while let Some(open_at) = rest.find(TOOL_USE_OPEN) {
        let before = &rest[..open_at];
        let after_open = &rest[open_at..];
        let parsed = parse_tool_use_block(after_open);
        match parsed {
            Some((tool_use, consumed)) => {
                if !before.trim().is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: before.trim().to_string(),
                    });
                }
                blocks.push(tool_use);
                rest = &after_open[consumed..];
            }
            None => {
                // 非法块：保留 `<tool_use` 起的一个字符为文本，继续向后扫描
                let split = open_at + TOOL_USE_OPEN.len();
                if !rest[..split].trim().is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: rest[..split].to_string(),
                    });
                }
                rest = &rest[split..];
            }
        }
    }
    if !rest.trim().is_empty() {
        blocks.push(ContentBlock::Text {
            text: rest.trim().to_string(),
        });
    }
    if blocks.is_empty() && !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }
    blocks
}

/// 解析以 `<tool_use` 开头的协议块；成功返回 (block, 消耗的字节数)。
fn parse_tool_use_block(input: &str) -> Option<(ContentBlock, usize)> {
    let header_end = input.find('>')?;
    let header = &input[TOOL_USE_OPEN.len()..header_end];
    let id = parse_attr(header, "id")?;
    let name = parse_attr(header, "name")?;
    let body_start = header_end + 1;
    let close_at = input[body_start..].find(TOOL_USE_CLOSE)?;
    let body = input[body_start..body_start + close_at].trim();
    let parsed: Value = if body.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(body).ok()?
    };
    let consumed = body_start + close_at + TOOL_USE_CLOSE.len();
    Some((
        ContentBlock::ToolUse {
            id,
            name,
            input: parsed,
        },
        consumed,
    ))
}

/// 从 `id="..."` 形式的属性串中提取值（手写解析，避免 wasm 端引入 regex）。
fn parse_attr(header: &str, key: &str) -> Option<String> {
    let marker = format!("{key}=\"");
    let start = header.find(&marker)? + marker.len();
    let end = header[start..].find('"')?;
    let value = &header[start..start + end];
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
}

// ──────────────────────────────────────────────
//  UI delta 过滤：抑制协议标签外漏
// ──────────────────────────────────────────────

/// 流式 delta 的协议标签过滤状态机：`<tool_use ... </tool_use>` 之间的
/// 内容不透传 UI；标签可跨 delta 分片，尾部潜在前缀先扣留。
pub struct ToolTagFilter {
    carry: String,
    in_tag: bool,
}

impl Default for ToolTagFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolTagFilter {
    pub fn new() -> Self {
        Self {
            carry: String::new(),
            in_tag: false,
        }
    }

    /// 输入一段增量，返回可安全展示的文本。
    pub fn push(&mut self, delta: &str) -> String {
        let mut input = std::mem::take(&mut self.carry);
        input.push_str(delta);
        let mut visible = String::new();
        loop {
            if self.in_tag {
                match input.find(TOOL_USE_CLOSE) {
                    Some(at) => {
                        input = input[at + TOOL_USE_CLOSE.len()..].to_string();
                        self.in_tag = false;
                    }
                    None => {
                        // 丢弃已确认的标签内内容，仅保留可能的闭合标签前缀
                        self.carry = trailing_prefix_of(&input, TOOL_USE_CLOSE);
                        return visible;
                    }
                }
            } else {
                match input.find(TOOL_USE_OPEN) {
                    Some(at) => {
                        let after = at + TOOL_USE_OPEN.len();
                        match input[after..].chars().next() {
                            // 紧跟属性分隔空白或 `>` 才是真协议开标签；避免
                            // `<tool_used>` 之类相似标签误入 in_tag 吞掉后续文本
                            Some(c) if c.is_ascii_whitespace() || c == '>' => {
                                visible.push_str(&input[..at]);
                                input = input[after..].to_string();
                                self.in_tag = true;
                            }
                            // 后随其它字符：误报，整段按普通文本放行并继续扫描
                            Some(_) => {
                                visible.push_str(&input[..after]);
                                input = input[after..].to_string();
                            }
                            // 恰好断在 `<tool_use` 结尾：扣留整个标记等下一分片定夺
                            None => {
                                visible.push_str(&input[..at]);
                                self.carry = input[at..].to_string();
                                return visible;
                            }
                        }
                    }
                    None => {
                        let held = trailing_prefix_of(&input, TOOL_USE_OPEN);
                        let emit_len = input.len() - held.len();
                        visible.push_str(&input[..emit_len]);
                        self.carry = held;
                        return visible;
                    }
                }
            }
        }
    }

    /// 流结束时冲刷扣留内容（仅 Normal 态的误报前缀需要归还）。
    pub fn flush(&mut self) -> String {
        if self.in_tag {
            self.carry.clear();
            return String::new();
        }
        std::mem::take(&mut self.carry)
    }
}

/// 返回 `input` 末尾与 `marker` 前缀重叠的最长后缀（跨分片标签检测）。
fn trailing_prefix_of(input: &str, marker: &str) -> String {
    let max = marker.len().saturating_sub(1).min(input.len());
    for len in (1..=max).rev() {
        if !input.is_char_boundary(input.len() - len) {
            continue;
        }
        let tail = &input[input.len() - len..];
        if marker.starts_with(tail) {
            return tail.to_string();
        }
    }
    String::new()
}

// ──────────────────────────────────────────────
//  重试判定与退避
// ──────────────────────────────────────────────

/// 可重试判定（对齐基线 `_is_retryable`）：网络错误 / 可重试状态码；
/// 认证与请求形状错误不重试。
fn is_retryable(error: &ClientError) -> bool {
    match error {
        ClientError::Network(_) | ClientError::RateLimited(_) => true,
        ClientError::ServerError(status, _) => RETRYABLE_STATUS_CODES.contains(status),
        ClientError::Api { status, .. } => RETRYABLE_STATUS_CODES.contains(status),
        _ => false,
    }
}

/// 流内失败（Failed 信封 / error 事件）的不可重试错误码：服务端
/// `HttpError.error_type` 全集中的终态子集（认证/请求形状/配额类，
/// 见 `ains-runtime::error`）。流内无 HTTP 状态码，只能按码判定。
pub const NON_RETRYABLE_STREAM_CODES: [&str; 9] = [
    "bad_request",
    "unauthorized",
    "forbidden",
    "not_found",
    "conflict",
    "validation_error",
    "insufficient_balance",
    "no_active_plan",
    "upstream_rejected",
];

fn is_non_retryable_stream_code(code: &str) -> bool {
    NON_RETRYABLE_STREAM_CODES.contains(&code)
}

/// 指数退避 + 抖动（对齐基线 `_get_retry_delay`；抖动源为毫秒时钟的
/// 均匀映射，客户端无 RNG 依赖）。
///
/// 防御性说明：`now_ms()` 在系统时钟不可用时回退 0，此时抖动恒为 0、
/// 并发客户端将以相同间隔重试（理论羊群效应）。时钟失败极罕见且指数
/// 退避本身仍在拉开间隔，接受该退化，不引入 RNG 依赖。
fn retry_delay(attempt: u32) -> f32 {
    let delay = (BASE_DELAY_SECS * 2f32.powi(attempt as i32)).min(MAX_DELAY_SECS);
    let jitter = (now_ms().rem_euclid(1000) as f32 / 1000.0) * delay * 0.25;
    delay + jitter
}

/// 不可重试/重试耗尽的终止事件（trait 无错误事件变体，以 Retry 事件承载
/// 最终失败信息后终止流；Kernel 对无 Complete 的流归一为 recoverable Error，
/// 并按 `attempt >= max_attempts` 识别终态不渲染为“重试中”）。
fn terminal_failure_event(message: String) -> ModelStreamEvent {
    ModelStreamEvent::Retry {
        message,
        attempt: MAX_RETRIES + 1,
        max_attempts: MAX_RETRIES + 1,
        delay_secs: 0.0,
    }
}

fn retry_exhausted_event(error: &ClientError) -> ModelStreamEvent {
    terminal_failure_event(format!("request failed: {error}"))
}

fn map_client_error(error: ClientError) -> AgentError {
    AgentError::Model(error.to_string())
}

/// 按魔数嗅探音频格式（`ModelClient::stt` 只有字节入参）；未知回退 wav。
pub fn detect_audio_format(data: &[u8]) -> &'static str {
    if data.starts_with(b"RIFF") {
        "wav"
    } else if data.starts_with(b"fLaC") {
        "flac"
    } else if data.starts_with(b"OggS") {
        "ogg"
    } else if data.starts_with(b"ID3")
        || data.starts_with(&[0xFF, 0xFB])
        || data.starts_with(&[0xFF, 0xF3])
    {
        "mp3"
    } else if data.len() > 11 && &data[4..8] == b"ftyp" {
        "mp4"
    } else if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        "webm"
    } else {
        "wav"
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_assistant_content_extracts_tool_use_blocks() {
        let text = "我来查一下。\n<tool_use id=\"call_1\" name=\"calculator\">\n{\"expr\": \"1+1\"}\n</tool_use>\n稍等。";
        let blocks = parse_assistant_content(text);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "我来查一下。"));
        match &blocks[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "calculator");
                assert_eq!(input, &json!({"expr": "1+1"}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        assert!(matches!(&blocks[2], ContentBlock::Text { text } if text == "稍等。"));
    }

    #[test]
    fn parse_assistant_content_keeps_malformed_block_as_text() {
        let text = "<tool_use id=\"x\" name=\"y\">\nnot json\n</tool_use>";
        let blocks = parse_assistant_content(text);
        // 非法 JSON：整段保留为文本，不产生 ToolUse
        assert!(
            blocks
                .iter()
                .all(|b| matches!(b, ContentBlock::Text { .. }))
        );
        let merged: String = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(merged.contains("not json"));
    }

    #[test]
    fn parse_assistant_content_plain_text_passthrough() {
        let blocks = parse_assistant_content("你好，世界");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "你好，世界"));
    }

    #[test]
    fn parse_tool_use_block_missing_name_fails_open_to_text() {
        // 缺少 name 属性 → parse_attr 返回 None → 整块保留为文本，不产生 ToolUse
        let blocks = parse_assistant_content("<tool_use id=\"c1\">\n{\"a\":1}\n</tool_use>");
        assert!(
            blocks
                .iter()
                .all(|b| matches!(b, ContentBlock::Text { .. })),
            "missing name must not yield a ToolUse: {blocks:?}"
        );
    }

    #[test]
    fn parse_tool_use_block_without_close_tag_fails_open_to_text() {
        // 有合法头但无 </tool_use> 闭合 → find close 返回 None → fail-open 为文本
        let blocks = parse_assistant_content("<tool_use id=\"c1\" name=\"calc\">\n{\"a\":1}");
        assert!(
            blocks
                .iter()
                .all(|b| matches!(b, ContentBlock::Text { .. }))
        );
        let merged: String = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(merged.contains("calc"));
    }

    #[test]
    fn tool_tag_filter_suppresses_protocol_across_chunks() {
        let mut filter = ToolTagFilter::new();
        let mut visible = String::new();
        // 标签跨三个分片
        visible.push_str(&filter.push("答案是 <tool"));
        visible.push_str(&filter.push("_use id=\"c1\" name=\"calc\">{\"a\":1}</tool_"));
        visible.push_str(&filter.push("use>，请稍候"));
        visible.push_str(&filter.flush());
        assert_eq!(visible, "答案是 ，请稍候");
    }

    #[test]
    fn tool_tag_filter_flushes_false_positive_prefix() {
        let mut filter = ToolTagFilter::new();
        let mut visible = String::new();
        visible.push_str(&filter.push("小于号 <tool"));
        // 后续不是协议标签
        visible.push_str(&filter.push("box 组件"));
        visible.push_str(&filter.flush());
        assert_eq!(visible, "小于号 <toolbox 组件");
    }

    #[test]
    fn tool_tag_filter_does_not_swallow_similar_tag_name() {
        // `<tool_used>` 与协议开标签同前缀但非协议标签：不得进入 in_tag
        // 吞掉后续文本（否则无 </tool_use> 闭合时余下内容全部丢失）
        let mut filter = ToolTagFilter::new();
        let mut visible = String::new();
        visible.push_str(&filter.push("见 <tool_used> 标记后的正文"));
        visible.push_str(&filter.flush());
        assert_eq!(visible, "见 <tool_used> 标记后的正文");
    }

    #[test]
    fn tool_tag_filter_similar_tag_split_across_chunks_is_not_swallowed() {
        // 分片恰好断在 `<tool_use` 结尾，下一分片揭示其实是 `<tool_used>`
        let mut filter = ToolTagFilter::new();
        let mut visible = String::new();
        visible.push_str(&filter.push("前缀 <tool_use"));
        visible.push_str(&filter.push("d> 后续文本"));
        visible.push_str(&filter.flush());
        assert_eq!(visible, "前缀 <tool_used> 后续文本");
    }

    #[test]
    fn tool_tag_filter_open_tag_split_after_marker_still_suppresses() {
        // 分片断在 `<tool_use` 结尾但后续确为协议标签：仍须抑制
        let mut filter = ToolTagFilter::new();
        let mut visible = String::new();
        visible.push_str(&filter.push("答案 <tool_use"));
        visible.push_str(&filter.push(" id=\"c1\" name=\"calc\">{}</tool_use>尾声"));
        visible.push_str(&filter.flush());
        assert_eq!(visible, "答案 尾声");
    }

    #[test]
    fn tool_tag_filter_multiple_blocks_in_single_push() {
        // 单次 push 内含多个完整协议块：loop 逐块抑制，块间文本全部放行
        let mut filter = ToolTagFilter::new();
        let mut visible = filter.push(
            "甲<tool_use id=\"1\" name=\"x\">{}</tool_use>乙\
             <tool_use id=\"2\" name=\"y\">{\"k\":1}</tool_use>丙",
        );
        visible.push_str(&filter.flush());
        assert_eq!(visible, "甲乙丙");
    }

    #[test]
    fn render_messages_bridges_tool_blocks_to_protocol_text() {
        let messages = vec![
            ConversationMessage {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "查一下".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "calc".into(),
                        input: json!({"a": 1}),
                    },
                ],
            },
            ConversationMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "2".into(),
                    is_error: false,
                    result_metadata: Value::Null,
                }],
            },
        ];
        let rendered = render_messages(&messages);
        assert_eq!(rendered.len(), 2);
        let assistant_text = match &rendered[0].content {
            client_api::AiContent::Text(text) => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(assistant_text.contains("<tool_use id=\"c1\" name=\"calc\">"));
        let user_text = match &rendered[1].content {
            client_api::AiContent::Text(text) => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(user_text.contains("<tool_result id=\"c1\" is_error=\"false\">"));
    }

    #[test]
    fn render_messages_images_become_data_uri_parts() {
        let messages = vec![ConversationMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "看图".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ],
        }];
        let rendered = render_messages(&messages);
        let client_api::AiContent::Parts(parts) = &rendered[0].content else {
            panic!("expected parts");
        };
        assert!(matches!(&parts[0], AiContentPart::InputText { text } if text == "看图"));
        assert!(matches!(
            &parts[1],
            AiContentPart::InputImage { image_url, .. }
                if image_url == "data:image/png;base64,AAAA"
        ));
    }

    #[test]
    fn build_ai_request_injects_tool_protocol_into_instructions() {
        let request = ModelRequest {
            system_prompt: Some("base".into()),
            tools: vec![crate::tools::ToolDef {
                name: "calc".into(),
                description: "calculator".into(),
                input_schema: json!({"type": "object"}),
            }],
            ..Default::default()
        };
        let ai_request = build_ai_request(&request);
        let instructions = ai_request.instructions.unwrap();
        assert!(instructions.starts_with("base"));
        assert!(instructions.contains("# Tool Call Protocol"));
        assert!(instructions.contains("- calc: calculator"));
        // 无工具时不注入协议段
        let bare = build_ai_request(&ModelRequest {
            system_prompt: Some("base".into()),
            ..Default::default()
        });
        assert_eq!(bare.instructions.as_deref(), Some("base"));
    }

    #[test]
    fn retry_delay_is_bounded_with_jitter() {
        for attempt in 0..6 {
            let delay = retry_delay(attempt);
            let base = (BASE_DELAY_SECS * 2f32.powi(attempt as i32)).min(MAX_DELAY_SECS);
            assert!(delay >= base);
            assert!(delay <= base * 1.25 + f32::EPSILON);
        }
    }

    #[test]
    fn is_retryable_matches_baseline_status_codes() {
        assert!(is_retryable(&ClientError::Network("x".into())));
        assert!(is_retryable(&ClientError::RateLimited("x".into())));
        assert!(is_retryable(&ClientError::Api {
            status: 529,
            code: "overloaded".into(),
            message: String::new(),
        }));
        assert!(!is_retryable(&ClientError::Api {
            status: 401,
            code: "unauthorized".into(),
            message: String::new(),
        }));
        assert!(!is_retryable(&ClientError::Api {
            status: 403,
            code: "no_active_plan".into(),
            message: String::new(),
        }));
        assert!(!is_retryable(&ClientError::Deserialization("x".into())));
    }

    #[test]
    fn detect_audio_format_sniffs_magic_bytes() {
        assert_eq!(detect_audio_format(b"RIFF....WAVE"), "wav");
        assert_eq!(detect_audio_format(b"fLaC...."), "flac");
        assert_eq!(detect_audio_format(b"OggS...."), "ogg");
        assert_eq!(detect_audio_format(b"ID3....."), "mp3");
        assert_eq!(detect_audio_format(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0]), "webm");
        assert_eq!(detect_audio_format(b"....ftypisom"), "mp4");
        assert_eq!(detect_audio_format(b"unknown"), "wav");
    }
}
