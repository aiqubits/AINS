//! 上下文压缩四级降级链（Phase 5.5，对齐 Harness `services/compact`）。
//!
//! 降级链（逐级执行，每级后复查阈值，达标即返回）：
//! 1. microcompact：清除旧的可压缩工具结果（保留最近 N 个），无 LLM 调用；
//! 2. 文本折叠：老段超长 Text/ToolResult 折叠为 head+标记+tail；
//! 3. 会话记忆压缩：老段压成单条摘要消息，无 LLM 调用；
//! 4. LLM 全量摘要：调用 `ModelClient` 产出结构化摘要替换老段。
//!
//! 触发源 auto/manual/reactive（[`CompactTrigger`]）；token 估算含图像预算；
//! tool_use/tool_result 配对保护切分；连续失败熔断；进度经回调上报（供 Kernel
//! 转 `CompactProgress` 事件）。

use crate::kernel::messages::{
    ContentBlock, ConversationMessage, Role, sanitize_conversation_messages,
};
use crate::kernel::state::CompactTrigger;
use crate::model_client::{ModelClient, ModelRequest, ModelStreamEvent};
pub use crate::prompts::COMPACT_PROMPT;
use crate::prompts::COMPACTION_SYSTEM_PROMPT;

use futures::StreamExt;

// ── 常量（逐字对齐基线 `services/compact/__init__.py`）──

/// 可 microcompact 的工具名集合（基线 `COMPACTABLE_TOOLS`）。
pub const COMPACTABLE_TOOLS: [&str; 8] = [
    "read_file",
    "bash",
    "grep",
    "glob",
    "web_search",
    "web_fetch",
    "edit_file",
    "write_file",
];
/// 清除占位文本（基线 `TIME_BASED_MC_CLEARED_MESSAGE`）。
pub const CLEARED_TOOL_RESULT: &str = "[Old tool result content cleared]";
/// 自动压缩缓冲（基线 `AUTOCOMPACT_BUFFER_TOKENS`）。
pub const AUTOCOMPACT_BUFFER_TOKENS: u64 = 13_000;
/// 摘要输出 token 上限（基线 `MAX_OUTPUT_TOKENS_FOR_SUMMARY`）。
pub const MAX_OUTPUT_TOKENS_FOR_SUMMARY: u32 = 20_000;
/// 连续失败熔断次数（基线 `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES`）。
pub const MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES: u32 = 3;
/// LLM 摘要流式重试次数（基线 `MAX_COMPACT_STREAMING_RETRIES`）。
pub const MAX_COMPACT_STREAMING_RETRIES: u32 = 2;
/// prompt-too-long 头部截断重试次数（基线 `MAX_PTL_RETRIES`）。
pub const MAX_PTL_RETRIES: u32 = 3;
/// 会话记忆压缩保留最近消息数（基线 `SESSION_MEMORY_KEEP_RECENT`）。
pub const SESSION_MEMORY_KEEP_RECENT: usize = 12;
/// 会话记忆最大行数 / 字符数（基线 `SESSION_MEMORY_MAX_*`）。
pub const SESSION_MEMORY_MAX_LINES: usize = 48;
pub const SESSION_MEMORY_MAX_CHARS: usize = 4_000;
/// 文本折叠阈值 / 头尾保留字符数（基线 `CONTEXT_COLLAPSE_*`）。
pub const CONTEXT_COLLAPSE_TEXT_CHAR_LIMIT: usize = 2_400;
pub const CONTEXT_COLLAPSE_HEAD_CHARS: usize = 900;
pub const CONTEXT_COLLAPSE_TAIL_CHARS: usize = 500;
/// microcompact 默认保留最近工具结果数（基线 `DEFAULT_KEEP_RECENT`）。
pub const DEFAULT_KEEP_RECENT: usize = 5;
/// LLM 摘要默认保留最近消息数（基线 `preserve_recent = 6`）。
pub const DEFAULT_PRESERVE_RECENT: usize = 6;
/// token 估算保守 padding（基线 `TOKEN_ESTIMATION_PADDING = 4/3`）。
pub const TOKEN_ESTIMATION_NUM: u64 = 4;
pub const TOKEN_ESTIMATION_DEN: u64 = 3;
/// 每张图像 token 预算（基线默认 `_DEFAULT_VISION_IMAGE_TOKEN_ESTIMATE`）。
pub const VISION_IMAGE_TOKEN_ESTIMATE: u64 = 3_072;
/// 默认上下文窗口（基线 `_DEFAULT_CONTEXT_WINDOW`）。
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
/// PTL 重试的头部截断标记（基线 `PTL_RETRY_MARKER`）。
pub const PTL_RETRY_MARKER: &str = "[earlier conversation truncated for compaction retry]";

// ── token 估算（对齐 `services/token_estimation.py` + compact 图像预算）──

/// 纯文本 token 估算：`max(1, (len+3)/4)`（基线 `estimate_tokens`）。
///
/// 有意偏差：`len` 为 UTF-8 字节数而非基线的字符数，CJK 文本（每字约
/// 3 字节）会高估约 3 倍。方向保守安全（提前而非延后触发压缩），且
/// CJK 实际 tokenizer 每字约 1–2 token，字节计数反而更贴近真实开销；
/// O(1) 无需扫描字符，接受该偏差。
pub fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    ((text.len() as u64).div_ceil(4)).max(1)
}

/// 会话 token 估算（含图像预算 + 4/3 padding，基线 `estimate_message_tokens`）。
pub fn estimate_message_tokens(messages: &[ConversationMessage]) -> u64 {
    let mut total: u64 = 0;
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => total += estimate_tokens(text),
                ContentBlock::ToolResult { content, .. } => total += estimate_tokens(content),
                ContentBlock::ToolUse { name, input, .. } => {
                    total += estimate_tokens(name);
                    total += estimate_tokens(&input.to_string());
                }
                ContentBlock::Image { .. } => total += VISION_IMAGE_TOKEN_ESTIMATE,
            }
        }
    }
    total * TOKEN_ESTIMATION_NUM / TOKEN_ESTIMATION_DEN
}

/// 上下文窗口（客户端保守取默认值；模型名保留供未来家族区分）。
pub fn get_context_window(_model: Option<&str>) -> u64 {
    DEFAULT_CONTEXT_WINDOW
}

/// 自动压缩阈值（基线 `get_autocompact_threshold`）：
/// 窗口 − min(20000, 20000) − 13000。
pub fn get_autocompact_threshold(model: Option<&str>) -> u64 {
    let window = get_context_window(model);
    let reserved = (MAX_OUTPUT_TOKENS_FOR_SUMMARY as u64).min(20_000);
    window
        .saturating_sub(reserved)
        .saturating_sub(AUTOCOMPACT_BUFFER_TOKENS)
}

/// 跨查询轮持久的自动压缩状态（基线 `AutoCompactState`）。
#[derive(Debug, Clone, Default)]
pub struct AutoCompactState {
    pub compacted: bool,
    pub turn_counter: u64,
    pub consecutive_failures: u32,
}

/// 是否应触发自动压缩（基线 `should_autocompact`）：连续失败达上限熔断。
pub fn should_autocompact(
    messages: &[ConversationMessage],
    model: Option<&str>,
    state: &AutoCompactState,
) -> bool {
    if state.consecutive_failures >= MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES {
        return false;
    }
    estimate_message_tokens(messages) >= get_autocompact_threshold(model)
}

// ── 配对保护切分（基线 `_split_preserving_tool_pairs`）──

/// 切点是否劈开 tool_use/tool_result 配对（基线 `_boundary_crosses_tool_pair`）。
fn boundary_crosses_tool_pair(
    previous: &ConversationMessage,
    current: &ConversationMessage,
) -> bool {
    if previous.role != Role::Assistant || current.role != Role::User {
        return false;
    }
    let pending: std::collections::HashSet<&str> = previous
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    if pending.is_empty() {
        return false;
    }
    current.content.iter().any(|b| match b {
        ContentBlock::ToolResult { tool_use_id, .. } => pending.contains(tool_use_id.as_str()),
        _ => false,
    })
}

/// 切分老段/新段，切点不劈开工具配对，新段再 sanitize（基线同名函数）。
pub fn split_preserving_tool_pairs(
    messages: &[ConversationMessage],
    preserve_recent: usize,
) -> (Vec<ConversationMessage>, Vec<ConversationMessage>) {
    if messages.len() <= preserve_recent {
        return (
            Vec::new(),
            sanitize_conversation_messages(messages.to_vec()),
        );
    }
    let mut split = messages.len() - preserve_recent;
    while split > 0 && boundary_crosses_tool_pair(&messages[split - 1], &messages[split]) {
        split -= 1;
    }
    let older = messages[..split].to_vec();
    let newer = sanitize_conversation_messages(messages[split..].to_vec());
    (older, newer)
}

// ── 第 1 级：microcompact（基线 `microcompact_messages`）──

/// 按出现顺序收集可压缩工具结果的 id（工具名在 `COMPACTABLE_TOOLS`）。
fn collect_compactable_tool_ids(messages: &[ConversationMessage]) -> Vec<String> {
    let mut ordered: Vec<String> = Vec::new();
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                ordered.push(id.clone());
                names.insert(id.clone(), name.clone());
            }
        }
    }
    ordered
        .into_iter()
        .filter(|id| {
            names
                .get(id)
                .is_some_and(|name| COMPACTABLE_TOOLS.contains(&name.as_str()))
        })
        .collect()
}

/// 清除旧的可压缩工具结果内容（保留最近 `keep_recent` 个），原地修改。
/// 返回节省的 token 估算（基线 `microcompact_messages`）。
pub fn microcompact_messages(messages: &mut [ConversationMessage], keep_recent: usize) -> u64 {
    let keep_recent = keep_recent.max(1);
    let all_ids = collect_compactable_tool_ids(messages);
    if all_ids.len() <= keep_recent {
        return 0;
    }
    let keep_from = all_ids.len() - keep_recent;
    let clear_set: std::collections::HashSet<&str> =
        all_ids[..keep_from].iter().map(String::as_str).collect();

    let mut tokens_saved = 0u64;
    for message in messages.iter_mut() {
        if message.role != Role::User {
            continue;
        }
        for block in &mut message.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
                && clear_set.contains(tool_use_id.as_str())
                && content != CLEARED_TOOL_RESULT
            {
                tokens_saved += estimate_tokens(content);
                *content = CLEARED_TOOL_RESULT.to_string();
            }
        }
    }
    tokens_saved
}

// ── 第 2 级：文本折叠（基线 `try_context_collapse`）──

/// 折叠超长文本：≤ 阈值不动，否则 head(900) + 标记 + tail(500)。
fn collapse_text(text: &str) -> String {
    let char_count = text.chars().count();
    if char_count <= CONTEXT_COLLAPSE_TEXT_CHAR_LIMIT {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let omitted = char_count - CONTEXT_COLLAPSE_HEAD_CHARS - CONTEXT_COLLAPSE_TAIL_CHARS;
    let head: String = chars[..CONTEXT_COLLAPSE_HEAD_CHARS].iter().collect();
    let tail: String = chars[char_count - CONTEXT_COLLAPSE_TAIL_CHARS..]
        .iter()
        .collect();
    format!(
        "{}\n...[collapsed {omitted} chars]...\n{}",
        head.trim_end(),
        tail.trim_start()
    )
}

/// 折叠老段超长块；无变化或折叠后 token 不降则返回 `None`（基线同名函数）。
pub fn try_context_collapse(
    messages: &[ConversationMessage],
    preserve_recent: usize,
) -> Option<Vec<ConversationMessage>> {
    if messages.len() <= preserve_recent + 2 {
        return None;
    }
    let (older, newer) = split_preserving_tool_pairs(messages, preserve_recent);
    let mut changed = false;
    let mut collapsed_older = Vec::with_capacity(older.len());
    for message in older {
        let mut new_blocks = Vec::with_capacity(message.content.len());
        for block in message.content {
            match block {
                ContentBlock::Text { text } => {
                    let collapsed = collapse_text(&text);
                    if collapsed != text {
                        changed = true;
                    }
                    new_blocks.push(ContentBlock::Text { text: collapsed });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    result_metadata,
                } => {
                    let collapsed = collapse_text(&content);
                    if collapsed != content {
                        changed = true;
                    }
                    new_blocks.push(ContentBlock::ToolResult {
                        tool_use_id,
                        content: collapsed,
                        is_error,
                        result_metadata,
                    });
                }
                other => new_blocks.push(other),
            }
        }
        collapsed_older.push(ConversationMessage {
            role: message.role,
            content: new_blocks,
        });
    }
    if !changed {
        return None;
    }
    let mut result = collapsed_older;
    result.extend(newer);
    if estimate_message_tokens(&result) >= estimate_message_tokens(messages) {
        return None;
    }
    Some(result)
}

// ── 第 3 级：会话记忆压缩（基线 `try_session_memory_compaction`）──

/// 单条消息压成一行摘要（基线 `_summarize_message_for_memory`）。
fn summarize_message_for_memory(message: &ConversationMessage) -> String {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let text: String = message
        .text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !text.is_empty() {
        let capped: String = text.chars().take(160).collect();
        return format!("{role}: {capped}");
    }
    let tools: Vec<String> = message
        .tool_uses()
        .into_iter()
        .map(|u| u.name)
        .take(4)
        .collect();
    if !tools.is_empty() {
        return format!("{role}: tool calls -> {}", tools.join(", "));
    }
    if message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    {
        return format!("{role}: tool results returned");
    }
    format!("{role}: [non-text content]")
}

/// 老段压成单条会话记忆消息（行数/字符预算内，基线 `_build_session_memory_message`）。
fn build_session_memory_message(messages: &[ConversationMessage]) -> Option<ConversationMessage> {
    let mut lines: Vec<String> = Vec::new();
    let mut total_chars = 0usize;
    for message in messages {
        let line = summarize_message_for_memory(message);
        let projected = total_chars + line.len() + 1;
        if !lines.is_empty()
            && (lines.len() >= SESSION_MEMORY_MAX_LINES || projected >= SESSION_MEMORY_MAX_CHARS)
        {
            lines.push("... earlier context condensed ...".to_string());
            break;
        }
        lines.push(line);
        total_chars = projected;
    }
    if lines.is_empty() {
        return None;
    }
    Some(ConversationMessage::from_user_text(format!(
        "Session memory summary from earlier in this conversation:\n{}",
        lines.join("\n")
    )))
}

/// 尝试会话记忆压缩（无 LLM）；提效不成立返回 `None`（基线同名函数）。
///
/// 接受条件逐字对齐基线的宽松语义（token 与消息数**同时**不降才拒绝，
/// `and` 连接）：老段极短时消息数下降但 token 反增的边缘结果仍会被
/// 接受——此时输入本就远低于阈值，不影响降级链正确性，保持与基线一致。
pub fn try_session_memory_compaction(
    messages: &[ConversationMessage],
    preserve_recent: usize,
) -> Option<Vec<ConversationMessage>> {
    if messages.len() <= preserve_recent + 4 {
        return None;
    }
    let (older, newer) = split_preserving_tool_pairs(messages, preserve_recent);
    let summary = build_session_memory_message(&older)?;
    let mut provisional = vec![summary];
    provisional.extend(newer);
    if estimate_message_tokens(&provisional) >= estimate_message_tokens(messages)
        && provisional.len() >= messages.len()
    {
        return None;
    }
    Some(sanitize_conversation_messages(provisional))
}

// ── 第 4 级：LLM 全量摘要（基线 `compact_conversation`）──

/// 抽取 `<summary>` 内容、剥离 `<analysis>`（基线 `format_compact_summary`）。
pub fn format_compact_summary(raw: &str) -> String {
    let mut text = strip_tag_block(raw, "analysis");
    if let Some(inner) = extract_tag_inner(&text, "summary") {
        // 用 "Summary:\n{inner}" 替换整个 <summary>...</summary>
        let block = format!("<summary>{inner}</summary>");
        text = text.replace(&block, &format!("Summary:\n{}", inner.trim()));
    }
    // 折叠多余空行
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text.trim().to_string()
}

/// 删除 `<tag>...</tag>`（含标签）整块。
fn strip_tag_block(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = text.to_string();
    while let (Some(start), Some(end)) = (out.find(&open), out.find(&close)) {
        if end < start {
            break;
        }
        out.replace_range(start..end + close.len(), "");
    }
    out
}

/// 提取 `<tag>...</tag>` 内部文本（首个匹配）。
fn extract_tag_inner(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].to_string())
}

/// 构建替换历史的摘要消息文本（基线 `build_compact_summary_message`）。
pub fn build_compact_summary_message(
    summary: &str,
    suppress_follow_up: bool,
) -> ConversationMessage {
    let formatted = format_compact_summary(summary);
    let mut text = format!(
        "This session is being continued from a previous conversation that ran out \
         of context. The summary below covers the earlier portion of the \
         conversation.\n\n{formatted}"
    );
    if suppress_follow_up {
        text.push_str(
            "\nContinue the conversation from where it left off without asking the \
             user any further questions. Resume directly.",
        );
    }
    ConversationMessage::from_user_text(text)
}

/// 图像块替换为文本占位（摘要请求前，避免向摘要模型重传图像）。
fn replace_images_with_placeholders(
    messages: Vec<ConversationMessage>,
) -> Vec<ConversationMessage> {
    messages
        .into_iter()
        .map(|message| ConversationMessage {
            role: message.role,
            content: message
                .content
                .into_iter()
                .map(|block| match block {
                    ContentBlock::Image { media_type, .. } => ContentBlock::Text {
                        text: format!("[image content omitted for compaction: {media_type}]"),
                    },
                    other => other,
                })
                .collect(),
        })
        .collect()
}

/// 按 prompt-round 分组（以"非 tool_result 的非空文本 user 消息"为轮次起点）。
fn group_by_prompt_round(messages: &[ConversationMessage]) -> Vec<Vec<ConversationMessage>> {
    let mut groups: Vec<Vec<ConversationMessage>> = Vec::new();
    let mut current: Vec<ConversationMessage> = Vec::new();
    for message in messages {
        let starts_new = message.role == Role::User
            && !message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            && !message.text().trim().is_empty();
        if starts_new && !current.is_empty() {
            groups.push(std::mem::take(&mut current));
        }
        current.push(message.clone());
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// PTL 重试的头部截断：丢弃最老的 `max(1, groups/5)` 组（基线同名函数）。
pub fn truncate_head_for_ptl_retry(
    messages: &[ConversationMessage],
) -> Option<Vec<ConversationMessage>> {
    let groups = group_by_prompt_round(messages);
    if groups.len() < 2 {
        return None;
    }
    let drop_count = (groups.len() / 5).max(1).min(groups.len() - 1);
    let retained: Vec<ConversationMessage> =
        groups[drop_count..].iter().flatten().cloned().collect();
    if retained.is_empty() {
        return None;
    }
    if retained[0].role == Role::Assistant {
        let mut with_marker = vec![ConversationMessage::from_user_text(PTL_RETRY_MARKER)];
        with_marker.extend(retained);
        return Some(with_marker);
    }
    Some(retained)
}

/// prompt-too-long 错误文本判定（基线 `_is_prompt_too_long_error` 子集）。
fn is_prompt_too_long(message: &str) -> bool {
    let text = message.to_lowercase();
    [
        "prompt too long",
        "context_length_exceeded",
        "context length",
        "maximum context",
        "context window",
        "input tokens exceed",
        "too many tokens",
        "exceeds the available context size",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// 摘要失败信息：`terminal` 为真表示 ModelClient 已判定不可重试（终态
/// Retry 事件 `attempt >= max_attempts`，如认证/配额/请求形状类错误），
/// compact 层不应再消耗流式重试预算无谓重发。
struct SummaryFailure {
    message: String,
    terminal: bool,
}

/// 调 LLM 收集一次摘要文本（取 Complete 事件；无完成返回 Err）。
async fn collect_summary(
    model: &dyn ModelClient,
    model_name: Option<&str>,
    request_messages: Vec<ConversationMessage>,
) -> Result<String, SummaryFailure> {
    let request = ModelRequest {
        model: model_name.map(str::to_string),
        messages: replace_images_with_placeholders(request_messages),
        system_prompt: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        max_output_tokens: MAX_OUTPUT_TOKENS_FOR_SUMMARY,
        tools: Vec::new(),
    };
    let mut stream = model
        .stream_response(request)
        .await
        .map_err(|e| SummaryFailure {
            message: e.to_string(),
            terminal: false,
        })?;
    let mut collected = String::new();
    let mut last_retry: Option<String> = None;
    let mut terminal = false;
    while let Some(event) = stream.next().await {
        match event {
            ModelStreamEvent::Complete { message, .. } => collected = message.text(),
            ModelStreamEvent::Retry {
                message,
                attempt,
                max_attempts,
                ..
            } => {
                last_retry = Some(message);
                // 终态 Retry（attempt 达上限）：ModelClient 已判定不可重试。
                terminal = attempt >= max_attempts;
            }
            ModelStreamEvent::TextDelta { .. } => {}
        }
    }
    if collected.trim().is_empty() {
        return Err(SummaryFailure {
            message: last_retry.unwrap_or_else(|| "compaction returned an empty summary".into()),
            terminal,
        });
    }
    Ok(collected)
}

/// 压缩进度回调（Kernel 传入以转 `CompactProgress` 事件）。
/// 压缩进度回调（Kernel 传入以转 `CompactProgress` 事件）。Native 端要求
/// `Send`，以保证持有该回调跨 await 的 `AgentKernel::run` future 仍可 `tokio::spawn`；
/// WASM 端单线程无此约束。
#[cfg(not(target_arch = "wasm32"))]
pub type ProgressFn<'a> = dyn FnMut(&str) + Send + 'a;
#[cfg(target_arch = "wasm32")]
pub type ProgressFn<'a> = dyn FnMut(&str) + 'a;

/// 压缩编排：四级降级链（基线 `auto_compact_if_needed`）。
///
/// 返回 `(messages, was_compacted)`。压缩失败递增熔断计数并原样返回消息。
///
/// 与基线的有意偏差（超越基线修复，已记录对齐清单）：
/// - 第 1/2 级成功早退同样重置 `consecutive_failures`（基线仅第 3/4 级
///   重置，违背“连续失败”语义，可能在有成功压缩穿插时提前熔断）；
/// - 第 4 级 passthrough 原样返回（如 ≤ preserve_recent 条巨型消息）
///   时不虚报成功，上报 `compact_noop` 供 UI 呈现“上下文无法继续压缩”。
#[allow(clippy::too_many_arguments)]
pub async fn auto_compact_if_needed(
    mut messages: Vec<ConversationMessage>,
    model: &dyn ModelClient,
    model_name: Option<&str>,
    state: &mut AutoCompactState,
    trigger: CompactTrigger,
    preserve_recent: usize,
    force: bool,
    progress: &mut ProgressFn<'_>,
) -> (Vec<ConversationMessage>, bool) {
    if !force && !should_autocompact(&messages, model_name, state) {
        return (messages, false);
    }

    // 第 1 级：microcompact
    let freed = microcompact_messages(&mut messages, DEFAULT_KEEP_RECENT);
    if freed > 0 && !force && !should_autocompact(&messages, model_name, state) {
        state.compacted = true;
        state.turn_counter += 1;
        state.consecutive_failures = 0;
        return (messages, true);
    }

    // 第 2 级：文本折叠
    if let Some(collapsed) = try_context_collapse(&messages, preserve_recent) {
        progress("context_collapse_start");
        messages = collapsed;
        progress("context_collapse_end");
        if !force && !should_autocompact(&messages, model_name, state) {
            state.compacted = true;
            state.turn_counter += 1;
            state.consecutive_failures = 0;
            return (messages, true);
        }
    }

    // 第 3 级：会话记忆压缩（无 LLM）
    if let Some(condensed) =
        try_session_memory_compaction(&messages, preserve_recent.max(SESSION_MEMORY_KEEP_RECENT))
    {
        progress("session_memory_start");
        progress("session_memory_end");
        state.compacted = true;
        state.turn_counter += 1;
        state.consecutive_failures = 0;
        return (condensed, true);
    }

    // 第 4 级：LLM 全量摘要
    progress("compact_start");
    match compact_conversation(
        &messages,
        model,
        model_name,
        preserve_recent,
        trigger,
        progress,
    )
    .await
    {
        Ok(compacted) => {
            // no-op 检测：compact_conversation 在 ≤ preserve_recent 或老段为空
            // 时 passthrough 原样返回（第 1 级已 microcompact，内部重跑幂等），
            // 此时不算压缩成功（不置 compacted、不动熔断计数），避免后续
            // 每轮徒劳重跑却汇报成功；上报 compact_noop 供 UI 呈现。
            if compacted == messages {
                progress("compact_noop");
                return (compacted, false);
            }
            progress("compact_end");
            state.compacted = true;
            state.turn_counter += 1;
            state.consecutive_failures = 0;
            (compacted, true)
        }
        Err(reason) => {
            state.consecutive_failures += 1;
            progress("compact_failed");
            let _ = reason;
            (messages, false)
        }
    }
}

/// LLM 摘要核心（基线 `compact_conversation`）：microcompact → 切分 → 组请求
/// → 流式（含 PTL 头部截断重试）。可直接调用（manual 触发），也由
/// [`auto_compact_if_needed`] 作为降级链末级调用。
pub async fn compact_conversation(
    messages: &[ConversationMessage],
    model: &dyn ModelClient,
    model_name: Option<&str>,
    preserve_recent: usize,
    trigger: CompactTrigger,
    progress: &mut ProgressFn<'_>,
) -> Result<Vec<ConversationMessage>, String> {
    if messages.len() <= preserve_recent {
        return Ok(messages.to_vec());
    }
    let mut working = messages.to_vec();
    microcompact_messages(&mut working, DEFAULT_KEEP_RECENT);
    let (older, newer) = split_preserving_tool_pairs(&working, preserve_recent);
    if older.is_empty() {
        return Ok(working);
    }

    let mut request_messages = older;
    request_messages.push(ConversationMessage::from_user_text(COMPACT_PROMPT));
    // PTL（prompt-too-long）头部截断与普通可重试失败各自独立计数，互不消耗：
    // PTL 至多 MAX_PTL_RETRIES 次（头部截断后重发），其余可重试失败至多
    // MAX_COMPACT_STREAMING_RETRIES 次。两者均在递增前检查上限，循环必定终止。
    let mut ptl_retries = 0u32;
    let mut stream_retries = 0u32;
    let summary = loop {
        match collect_summary(model, model_name, request_messages.clone()).await {
            Ok(text) => break text,
            Err(failure) => {
                if is_prompt_too_long(&failure.message) {
                    if ptl_retries >= MAX_PTL_RETRIES {
                        return Err(failure.message);
                    }
                    // 截断请求消息头部（保留末尾的 compact prompt 用户消息）
                    let body = &request_messages[..request_messages.len() - 1];
                    let Some(truncated) = truncate_head_for_ptl_retry(body) else {
                        return Err(failure.message);
                    };
                    ptl_retries += 1;
                    let mut next = truncated;
                    next.push(request_messages[request_messages.len() - 1].clone());
                    request_messages = next;
                    progress("compact_retry");
                    continue;
                }
                // 非 PTL 且 ModelClient 已判定终态不可重试（认证/配额/请求形状）：
                // 立即失败，不再消耗流式重试预算（避免对无望的摘要重发
                // MAX_COMPACT_STREAMING_RETRIES × (MAX_RETRIES+1) 次物理请求与退避延迟）。
                if failure.terminal {
                    return Err(failure.message);
                }
                if stream_retries >= MAX_COMPACT_STREAMING_RETRIES {
                    return Err(failure.message);
                }
                stream_retries += 1;
                progress("compact_retry");
            }
        }
    };
    let _ = trigger;
    let mut result = vec![build_compact_summary_message(&summary, true)];
    result.extend(newer);
    Ok(sanitize_conversation_messages(result))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::error::AgentError;
    use crate::kernel::mock_model::ScriptedModelClient;
    use crate::model_client::UsageSnapshot;
    use serde_json::json;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::from_user_text(text)
    }

    fn assistant_tool(id: &str, name: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: json!({}),
            }],
        }
    }

    fn tool_result(id: &str, content: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: false,
                result_metadata: serde_json::Value::Null,
            }],
        }
    }

    #[test]
    fn estimate_tokens_matches_char_heuristic() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn estimate_message_tokens_includes_image_budget_and_padding() {
        let messages = vec![ConversationMessage {
            role: Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
        }];
        // 单图：3072 * 4/3 = 4096
        assert_eq!(
            estimate_message_tokens(&messages),
            VISION_IMAGE_TOKEN_ESTIMATE * 4 / 3
        );
    }

    #[test]
    fn autocompact_threshold_matches_baseline() {
        // 200000 - 20000 - 13000 = 167000
        assert_eq!(get_autocompact_threshold(None), 167_000);
    }

    #[test]
    fn should_autocompact_circuit_breaks_after_failures() {
        let big = vec![user(&"x".repeat(4 * 200_000))];
        let mut state = AutoCompactState::default();
        assert!(should_autocompact(&big, None, &state));
        state.consecutive_failures = MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES;
        assert!(!should_autocompact(&big, None, &state));
    }

    #[test]
    fn microcompact_clears_old_compactable_results_keeping_recent() {
        let mut messages = Vec::new();
        for i in 0..8 {
            messages.push(assistant_tool(&format!("c{i}"), "read_file"));
            messages.push(tool_result(&format!("c{i}"), &format!("file content {i}")));
        }
        let saved = microcompact_messages(&mut messages, 3);
        assert!(saved > 0);
        // 前 5 个被清除，后 3 个保留
        let cleared = messages
            .iter()
            .filter_map(|m| m.content.first())
            .filter(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content == CLEARED_TOOL_RESULT))
            .count();
        assert_eq!(cleared, 5);
    }

    #[test]
    fn microcompact_ignores_non_compactable_tools() {
        let mut messages = vec![
            assistant_tool("t1", "ask_user_question"),
            tool_result("t1", "answer"),
            assistant_tool("t2", "ask_user_question"),
            tool_result("t2", "answer2"),
        ];
        assert_eq!(microcompact_messages(&mut messages, 1), 0);
    }

    #[test]
    fn context_collapse_shrinks_oversized_text() {
        let long = "y".repeat(CONTEXT_COLLAPSE_TEXT_CHAR_LIMIT + 1000);
        let mut messages = vec![user(&long)];
        for i in 0..8 {
            messages.push(user(&format!("recent {i}")));
        }
        let collapsed = try_context_collapse(&messages, DEFAULT_PRESERVE_RECENT).unwrap();
        let first_text = collapsed[0].text();
        assert!(first_text.contains("...[collapsed"));
        assert!(estimate_message_tokens(&collapsed) < estimate_message_tokens(&messages));
    }

    #[test]
    fn split_preserving_tool_pairs_does_not_cut_pair() {
        // 末尾 6 条内切点会劈开 c/result 配对，切点应左移
        let messages = vec![
            user("goal"),
            user("filler1"),
            user("filler2"),
            assistant_tool("c1", "read_file"),
            tool_result("c1", "data"),
            user("more"),
        ];
        let (older, newer) = split_preserving_tool_pairs(&messages, 2);
        // 新段不以孤儿 tool_result 开头
        assert!(!matches!(
            newer.first().map(|m| &m.content[0]),
            Some(ContentBlock::ToolResult { .. })
        ));
        assert!(!older.is_empty());
    }

    #[test]
    fn split_preserving_tool_pairs_pair_shift_to_zero_returns_empty_older() {
        // 退化用例：唯一候选切点恰劈开配对，左移收敛到 0 —— 老段为空、
        // 不 panic 不死循环，新段保持配对完整
        let messages = vec![assistant_tool("c1", "read_file"), tool_result("c1", "data")];
        let (older, newer) = split_preserving_tool_pairs(&messages, 1);
        assert!(older.is_empty());
        assert_eq!(newer.len(), 2);
        assert!(matches!(&newer[0].content[0], ContentBlock::ToolUse { id, .. } if id == "c1"));
        assert!(
            matches!(&newer[1].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "c1")
        );
    }

    #[tokio::test]
    async fn compact_conversation_empty_older_after_pair_shift_is_passthrough() {
        // older 为空的 passthrough 分支（切点左移收敛到 0）：不调模型直接
        // 原样返回，上层 auto_compact_if_needed 据 CR-2 判为 no-op
        let messages = vec![assistant_tool("c1", "read_file"), tool_result("c1", "data")];
        let model = ScriptedModelClient::new(vec![]);
        let mut progress = |_: &str| {};
        let result = compact_conversation(
            &messages,
            &model,
            None,
            1,
            CompactTrigger::Manual,
            &mut progress,
        )
        .await
        .unwrap();
        assert_eq!(result, messages);
        assert!(
            model.recorded_requests().is_empty(),
            "must not call the model"
        );
    }

    #[test]
    fn session_memory_compaction_condenses_older() {
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(user(&format!("message number {i} with some content")));
        }
        let condensed =
            try_session_memory_compaction(&messages, SESSION_MEMORY_KEEP_RECENT).unwrap();
        assert!(condensed.len() < messages.len());
        assert!(condensed[0].text().contains("Session memory summary"));
    }

    #[test]
    fn session_memory_compaction_lenient_acceptance_matches_baseline() {
        // 锚定基线宽松语义：老段极短（每条 1 字符）时摘要模板开销占优，
        // token 反增但消息数下降（and 条件不成立）——仍接受，与基线一致。
        let messages: Vec<ConversationMessage> = (0..17).map(|_| user("a")).collect();
        let condensed =
            try_session_memory_compaction(&messages, SESSION_MEMORY_KEEP_RECENT).unwrap();
        assert!(condensed.len() < messages.len());
        assert!(
            estimate_message_tokens(&condensed) > estimate_message_tokens(&messages),
            "precondition: this edge case must actually increase tokens"
        );
    }

    #[test]
    fn format_compact_summary_extracts_summary_block() {
        let raw = "<analysis>scratch work here</analysis>\n<summary>\nDid the thing.\n</summary>";
        let formatted = format_compact_summary(raw);
        assert!(!formatted.contains("scratch work"));
        assert!(formatted.contains("Summary:"));
        assert!(formatted.contains("Did the thing."));
    }

    #[test]
    fn truncate_head_drops_oldest_rounds() {
        let mut messages = Vec::new();
        for i in 0..10 {
            messages.push(user(&format!("round {i}")));
        }
        let truncated = truncate_head_for_ptl_retry(&messages).unwrap();
        assert!(truncated.len() < messages.len());
        // 保留最新轮次
        assert!(truncated.last().unwrap().text().contains("round 9"));
    }

    #[tokio::test]
    async fn compact_conversation_replaces_history_with_summary() {
        // 直接测试第 4 级 LLM 摘要（绕过降级链前三级的拦截）
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(user(&format!("user request {i}")));
        }
        let summary_msg = ScriptedModelClient::assistant_text(
            "<analysis>looked</analysis><summary>All prior work summarized.</summary>",
        );
        let model = ScriptedModelClient::new(vec![ScriptedModelClient::turn(
            summary_msg,
            UsageSnapshot::default(),
        )]);
        let mut phases: Vec<String> = Vec::new();
        let result = compact_conversation(
            &messages,
            &model,
            Some("gpt-test"),
            DEFAULT_PRESERVE_RECENT,
            CompactTrigger::Manual,
            &mut |phase| phases.push(phase.to_string()),
        )
        .await
        .unwrap();
        assert!(result.len() < messages.len());
        assert!(
            result[0]
                .text()
                .contains("continued from a previous conversation")
        );
        assert!(result[0].text().contains("All prior work summarized."));
        // 最近 preserve_recent 条逐字保留
        assert!(result.last().unwrap().text().contains("request 19"));
    }

    #[tokio::test]
    async fn compact_conversation_errors_on_empty_summary() {
        let messages: Vec<ConversationMessage> =
            (0..20).map(|i| user(&format!("req {i}"))).collect();
        // 空脚本：stream_response 返回耗尽错误 → 摘要失败
        let model = ScriptedModelClient::new(vec![]);
        let result = compact_conversation(
            &messages,
            &model,
            Some("gpt-test"),
            DEFAULT_PRESERVE_RECENT,
            CompactTrigger::Manual,
            &mut |_| {},
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn auto_compact_reaches_session_memory_level_without_llm() {
        // 300 条短 user 消息：前两级不达标，第 3 级会话记忆压缩即可降到阈值下，
        // 不应调用 LLM（空脚本也不会 panic）。
        let messages: Vec<ConversationMessage> = (0..300)
            .map(|i| user(&format!("user request {i}: {}", "detail ".repeat(300))))
            .collect();
        let model = ScriptedModelClient::new(vec![]);
        let mut state = AutoCompactState::default();
        let mut phases: Vec<String> = Vec::new();
        let (result, compacted) = auto_compact_if_needed(
            messages.clone(),
            &model,
            Some("gpt-test"),
            &mut state,
            CompactTrigger::Auto,
            DEFAULT_PRESERVE_RECENT,
            false,
            &mut |phase| phases.push(phase.to_string()),
        )
        .await;
        assert!(compacted);
        assert!(result.len() < messages.len());
        // 第 3 级命中：会话记忆摘要，非 LLM 摘要
        assert!(result[0].text().contains("Session memory summary"));
        assert!(phases.iter().any(|p| p == "session_memory_end"));
        assert_eq!(state.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn auto_compact_skips_when_below_threshold() {
        let messages = vec![user("short conversation")];
        let model = ScriptedModelClient::new(vec![]);
        let mut state = AutoCompactState::default();
        let (result, compacted) = auto_compact_if_needed(
            messages.clone(),
            &model,
            None,
            &mut state,
            CompactTrigger::Auto,
            DEFAULT_PRESERVE_RECENT,
            false,
            &mut |_| {},
        )
        .await;
        assert!(!compacted);
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn force_bypasses_threshold_but_stops_at_first_successful_level() {
        // force 语义钉定（评审建议测试）：手动压缩绕过阈值门控，但降级链
        // 仍停在首个产出结果的层级（此处第 3 级会话记忆），而非必达第 4 级
        // LLM 摘要（空脚本模型被调用即 panic，反向钉住无 LLM 调用）
        let messages: Vec<ConversationMessage> =
            (0..20).map(|i| user(&format!("short note {i}"))).collect();
        assert!(
            !should_autocompact(&messages, None, &AutoCompactState::default()),
            "precondition: far below threshold"
        );
        let model = ScriptedModelClient::new(vec![]);
        let mut state = AutoCompactState::default();
        let mut phases: Vec<String> = Vec::new();
        let (result, compacted) = auto_compact_if_needed(
            messages.clone(),
            &model,
            None,
            &mut state,
            CompactTrigger::Manual,
            DEFAULT_PRESERVE_RECENT,
            true,
            &mut |phase| phases.push(phase.to_string()),
        )
        .await;
        assert!(compacted, "force must compact even below threshold");
        assert!(result.len() < messages.len());
        assert!(result[0].text().contains("Session memory summary"));
        assert!(phases.iter().any(|p| p == "session_memory_end"));
        assert!(
            !phases.iter().any(|p| p == "compact_start"),
            "must stop at level 3, not reach LLM summary: {phases:?}"
        );
        assert_eq!(state.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn low_level_success_resets_consecutive_failures() {
        // 回归（超越基线修复）：第 1 级 microcompact 成功早退曾不重置
        // consecutive_failures，导致成功压缩穿插在失败之间时提前熔断。
        // 构造：最老的可压缩工具结果巨大（清除后即降到阈值下），其余
        // 5 个（= DEFAULT_KEEP_RECENT，保留）很小。
        let mut messages = vec![
            assistant_tool("old", "read_file"),
            tool_result("old", &"z".repeat(4 * 200_000)),
        ];
        for i in 0..DEFAULT_KEEP_RECENT {
            messages.push(assistant_tool(&format!("r{i}"), "read_file"));
            messages.push(tool_result(&format!("r{i}"), "small"));
        }
        let mut state = AutoCompactState {
            consecutive_failures: 2,
            ..AutoCompactState::default()
        };
        assert!(should_autocompact(&messages, None, &state));

        // 空脚本：第 1 级即达标，不得触碰 LLM
        let model = ScriptedModelClient::new(vec![]);
        let (result, compacted) = auto_compact_if_needed(
            messages,
            &model,
            None,
            &mut state,
            CompactTrigger::Auto,
            DEFAULT_PRESERVE_RECENT,
            false,
            &mut |_| {},
        )
        .await;
        assert!(compacted);
        assert!(!should_autocompact(&result, None, &state));
        // 成功压缩后“连续失败”归零且计入压缩轮次
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.turn_counter, 1);
        assert!(state.compacted);
    }

    #[tokio::test]
    async fn few_huge_messages_noop_does_not_report_compacted() {
        // 回归（超越基线修复）：≤ preserve_recent 条巨型消息超阈值时，
        // 第 4 级 passthrough 原样返回曾虚报 was_compacted=true 并重置熔断，
        // 导致后续每轮徒劳重跑却汇报成功。
        let messages: Vec<ConversationMessage> = (0..3)
            .map(|i| user(&format!("huge {i}: {}", "y".repeat(4 * 100_000))))
            .collect();
        let mut state = AutoCompactState::default();
        assert!(should_autocompact(&messages, None, &state));

        // 空脚本：compact_conversation 在 len ≤ preserve_recent 时不调 LLM
        let model = ScriptedModelClient::new(vec![]);
        let mut phases: Vec<String> = Vec::new();
        let (result, compacted) = auto_compact_if_needed(
            messages.clone(),
            &model,
            None,
            &mut state,
            CompactTrigger::Auto,
            DEFAULT_PRESERVE_RECENT,
            false,
            &mut |p| phases.push(p.to_string()),
        )
        .await;
        assert!(!compacted, "passthrough must not be reported as compaction");
        assert_eq!(result.len(), messages.len());
        // 既非成功也非失败：熔断计数与 compacted 标记均不变
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.compacted);
        // 上报 compact_noop 供 UI 呈现“无法继续压缩”，不上报 compact_end
        assert!(phases.iter().any(|p| p == "compact_noop"));
        assert!(!phases.iter().any(|p| p == "compact_end"));
    }

    /// 构造 6 个可压缩工具配对（12 条消息）：低于自动阈值且第 1 级
    /// microcompact 必有清理量（freed > 0，keep_recent=5 < 6），同时消息数
    /// ≤ 16 使第 3 级会话记忆压缩返回 None，强制压缩必须穿透到第 4 级。
    fn small_conversation_with_clearable_tool_results() -> Vec<ConversationMessage> {
        let mut messages = Vec::new();
        for i in 0..6 {
            messages.push(assistant_tool(&format!("c{i}"), "read_file"));
            messages.push(tool_result(&format!("c{i}"), &format!("file content {i}")));
        }
        messages
    }

    #[tokio::test]
    async fn forced_compact_below_threshold_reaches_llm_summary() {
        // 回归（L608 缺 !force 守卫）：低于自动阈值但含可清理工具结果时，
        // force=true 曾在第 1 级 microcompact 后短路返回，永不到达第 4 级。
        let messages = small_conversation_with_clearable_tool_results();
        let mut state = AutoCompactState::default();
        assert!(!should_autocompact(&messages, Some("gpt-test"), &state));

        let summary_msg = ScriptedModelClient::assistant_text(
            "<analysis>reviewed</analysis><summary>Forced compact summary.</summary>",
        );
        let model = ScriptedModelClient::new(vec![ScriptedModelClient::turn(
            summary_msg,
            UsageSnapshot::default(),
        )]);
        let mut phases: Vec<String> = Vec::new();
        let (result, compacted) = auto_compact_if_needed(
            messages,
            &model,
            Some("gpt-test"),
            &mut state,
            CompactTrigger::Manual,
            DEFAULT_PRESERVE_RECENT,
            true,
            &mut |phase| phases.push(phase.to_string()),
        )
        .await;
        assert!(compacted);
        // 到达第 4 级：LLM 摘要阶段进度上报且摘要替换了老段
        assert!(phases.iter().any(|p| p == "compact_start"));
        assert!(phases.iter().any(|p| p == "compact_end"));
        assert!(result[0].text().contains("Forced compact summary."));
        assert_eq!(state.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn forced_compact_executes_full_chain_when_circuit_breaker_open() {
        // 回归（L608 缺 !force 守卫）：熔断开启（consecutive_failures=3）时
        // should_autocompact 恒为 false，force=true 的手动恢复路径曾在第 1 级
        // 短路并误报 was_compacted=true；应执行全链路并成功产出 LLM 摘要。
        let messages = small_conversation_with_clearable_tool_results();
        let mut state = AutoCompactState {
            consecutive_failures: MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES,
            ..AutoCompactState::default()
        };
        assert!(!should_autocompact(&messages, Some("gpt-test"), &state));

        let summary_msg = ScriptedModelClient::assistant_text(
            "<analysis>recovered</analysis><summary>Breaker recovery summary.</summary>",
        );
        let model = ScriptedModelClient::new(vec![ScriptedModelClient::turn(
            summary_msg,
            UsageSnapshot::default(),
        )]);
        let mut phases: Vec<String> = Vec::new();
        let (result, compacted) = auto_compact_if_needed(
            messages,
            &model,
            Some("gpt-test"),
            &mut state,
            CompactTrigger::Manual,
            DEFAULT_PRESERVE_RECENT,
            true,
            &mut |phase| phases.push(phase.to_string()),
        )
        .await;
        assert!(compacted);
        assert!(phases.iter().any(|p| p == "compact_start"));
        assert!(phases.iter().any(|p| p == "compact_end"));
        assert!(result[0].text().contains("Breaker recovery summary."));
        // 摘要成功后熔断计数归零（手动恢复路径重新打开自动压缩）
        assert_eq!(state.consecutive_failures, 0);
    }

    /// PTL（prompt-too-long）截断重试：首个响应报 PTL，头部截断后重试成功
    /// 产出 LLM 摘要（验证 #2：PTL 与流式重试预算解耦后 PTL 路径确实生效）。
    struct PtlThenOkModel {
        calls: std::sync::atomic::AtomicU32,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl ModelClient for PtlThenOkModel {
        async fn stream_response(
            &self,
            _request: ModelRequest,
        ) -> Result<crate::model_client::EventStream<ModelStreamEvent>, AgentError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events: Vec<ModelStreamEvent> = if n == 0 {
                // 首次：无 Complete，仅一个带 PTL 提示的 Retry 事件
                // → collect_summary 因无完成而报 PTL 错误
                vec![ModelStreamEvent::Retry {
                    message: "prompt too long: reduce the length of the messages".into(),
                    attempt: 1,
                    max_attempts: 4,
                    delay_secs: 0.0,
                }]
            } else {
                ScriptedModelClient::text_turn("PTL SUMMARY", UsageSnapshot::default())
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, AgentError> {
            Err(AgentError::Model("unused".into()))
        }
        async fn stt(&self, _audio: &[u8]) -> Result<String, AgentError> {
            Err(AgentError::Model("unused".into()))
        }
        async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
            Err(AgentError::Model("unused".into()))
        }
    }

    #[tokio::test]
    async fn compact_conversation_ptl_truncation_retry_succeeds() {
        // 足够多的独立 user 轮次，保证 truncate_head_for_ptl_retry 有 ≥2 组可截
        let messages: Vec<ConversationMessage> = (0..30)
            .map(|i| user(&format!("round {i} content")))
            .collect();
        let model = PtlThenOkModel {
            calls: std::sync::atomic::AtomicU32::new(0),
        };
        let mut phases: Vec<String> = Vec::new();
        let result = compact_conversation(
            &messages,
            &model,
            Some("gpt-test"),
            DEFAULT_PRESERVE_RECENT,
            CompactTrigger::Manual,
            &mut |p| phases.push(p.to_string()),
        )
        .await
        .unwrap();
        // 摘要成功（截断后第二次调用产出）
        assert!(result[0].text().contains("PTL SUMMARY"));
        // 恰好调用两次：首次 PTL，截断后第二次成功
        assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        // 触发过 compact_retry 进度
        assert!(phases.iter().any(|p| p == "compact_retry"));
    }

    /// 终态失败模型：每次调用都只产出一个终态 Retry（attempt == max_attempts），
    /// 模拟 ModelClient 对不可重试错误（如 no_active_plan）的终态上报。
    struct TerminalFailureModel {
        calls: std::sync::atomic::AtomicU32,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl ModelClient for TerminalFailureModel {
        async fn stream_response(
            &self,
            _request: ModelRequest,
        ) -> Result<crate::model_client::EventStream<ModelStreamEvent>, AgentError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = vec![ModelStreamEvent::Retry {
                message: "request failed: [no_active_plan] no active plan".into(),
                attempt: 4,
                max_attempts: 4,
                delay_secs: 0.0,
            }];
            Ok(Box::pin(futures::stream::iter(events)))
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, AgentError> {
            Err(AgentError::Model("unused".into()))
        }
        async fn stt(&self, _audio: &[u8]) -> Result<String, AgentError> {
            Err(AgentError::Model("unused".into()))
        }
        async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
            Err(AgentError::Model("unused".into()))
        }
    }

    #[tokio::test]
    async fn compact_conversation_terminal_failure_does_not_retry() {
        // 回归（超越基线修复）：非 PTL 的终态失败（ModelClient 已判定不可
        // 重试）不应再消耗流式重试预算；compact_conversation 应仅调用一次
        // 模型即失败（修复前会额外重试 MAX_COMPACT_STREAMING_RETRIES 次）。
        let messages: Vec<ConversationMessage> = (0..30)
            .map(|i| user(&format!("round {i} content")))
            .collect();
        let model = TerminalFailureModel {
            calls: std::sync::atomic::AtomicU32::new(0),
        };
        let result = compact_conversation(
            &messages,
            &model,
            Some("gpt-test"),
            DEFAULT_PRESERVE_RECENT,
            CompactTrigger::Manual,
            &mut |_| {},
        )
        .await;
        assert!(result.is_err(), "terminal failure must surface as error");
        assert!(result.unwrap_err().contains("no_active_plan"));
        // 关键：仅一次模型调用，无额外流式重试
        assert_eq!(
            model.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "terminal failure must not consume streaming-retry budget"
        );
    }
}
