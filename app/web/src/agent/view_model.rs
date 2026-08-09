//! `StreamEvent` → Chat 视图模型映射（Phase 6.3，纯函数层，无 Dioxus 依赖）。
//!
//! desktop 端经 `#[path]` 引用本文件复用同一实现与测试
//! （两端行为一致由本文件单测保障，见 AINS_PLAN Phase 6 计划 6.3）。

use agent_core::kernel::{
    ContentBlock, ConversationMessage, QUERY_INTERRUPTED_STATUS, Role, StreamEvent, ToolUse,
};
use serde_json::Value;
use ui::{ChatItem, ChatRole, ChatViewState, ToolCallStatus};

/// 恢复会话历史 → 初始 Chat 条目。
///
/// 仅落定 Text 内容；tool_use/tool_result block 无 UI 配对信息，渲染为已
/// 完成的工具卡片（输出以 tool_result 文本回填，找不到配对则留空）。
pub fn seed_history(messages: &[ConversationMessage]) -> Vec<ChatItem> {
    let mut items = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User => ChatRole::User,
            Role::Assistant => ChatRole::Assistant,
        };
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    if !text.trim().is_empty() {
                        // 用户消息若为 /skill 展开指令，恢复时渲染为别名，
                        // 避免整段 skill 正文倾泻进可见历史（与实时转写一致）
                        let display = (role == ChatRole::User)
                            .then(|| skill_display_alias(text))
                            .flatten()
                            .unwrap_or_else(|| text.clone());
                        items.push(ChatItem::Text {
                            role,
                            text: display,
                        });
                    }
                }
                ContentBlock::ToolUse { id, name, input } => {
                    let output = find_tool_result(messages, id);
                    items.push(ChatItem::ToolCall {
                        tool_use_id: id.clone(),
                        name: name.clone(),
                        input_preview: pretty_json(&mask_sensitive(input.clone())),
                        status: match &output {
                            Some((_, false)) => ToolCallStatus::Done,
                            // 错误结果或无配对结果（历史中断残留）均按失败展示
                            Some((_, true)) | None => ToolCallStatus::Failed,
                        },
                        output_preview: output.map(|(text, _)| text).unwrap_or_default(),
                    });
                }
                // 图像附件与 tool_result（已并入对应卡片）不单独渲染
                ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
            }
        }
    }
    items
}

fn find_tool_result(messages: &[ConversationMessage], tool_use_id: &str) -> Option<(String, bool)> {
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error,
                ..
            } = block
                && id == tool_use_id
            {
                return Some((content.clone(), *is_error));
            }
        }
    }
    None
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// 构造 `/skill` 展开指令（宿主发送给 Kernel 的完整 prompt）。
/// 与 [`skill_display_alias`] 配对：格式变更时两者必须同步。
pub fn skill_prompt(name: &str, body: &str) -> String {
    format!("Apply the skill `{name}` to the current task.\n\n<skill>\n{body}\n</skill>")
}

/// 若文本为 [`skill_prompt`] 生成的展开指令，返回 `/skill {name}`
/// 展示别名；否则返回 `None`（普通文本原样渲染）。
pub fn skill_display_alias(text: &str) -> Option<String> {
    let rest = text.strip_prefix("Apply the skill `")?;
    let (name, rest) = rest.split_once('`')?;
    rest.strip_prefix(" to the current task.\n\n<skill>\n")?;
    (rest.ends_with("\n</skill>") && !name.is_empty()).then(|| format!("/skill {name}"))
}

/// UI 展示前的敏感字段掩码：key 命中敏感词（大小写不敏感、含子串）时
/// 值替换为 `***`，递归处理嵌套对象/数组；字符串值内嵌的秘钥模式
/// （Bearer token / sk- 长密钥 / URL userinfo）另经 [`mask_embedded_secrets`]
/// 兜底掩码（仅掩秘钥本体，不影响用户审阅命令结构）。权限弹窗与
/// 聊天工具卡片共用同一掩码策略（避免 token/password 在存留会话中外泄）。
pub fn mask_sensitive(value: Value) -> Value {
    const SENSITIVE_KEY_PARTS: [&str; 8] = [
        "token",
        "password",
        "passwd",
        "secret",
        "authorization",
        "api_key",
        "apikey",
        "credential",
    ];
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, val)| {
                    let lower = key.to_lowercase();
                    if SENSITIVE_KEY_PARTS.iter().any(|part| lower.contains(part)) {
                        (key, Value::String("***".into()))
                    } else {
                        (key, mask_sensitive(val))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(mask_sensitive).collect()),
        Value::String(text) => Value::String(mask_embedded_secrets(&text)),
        other => other,
    }
}

/// 字符串值内嵌秘钥的模式掩码（key 级掩码的兜底：秘钥藏在非敏感
/// key 的值里，如 `command: "curl -H 'Authorization: Bearer …'"`）：
/// - `Bearer <token>` → `Bearer ***`（关键词大小写不敏感）
/// - `sk-` 前缀长密钥（≥20 位连续 [A-Za-z0-9_-]）→ `***`
/// - URL userinfo（`scheme://user:pass@host`）→ `scheme://***@host`
///
/// 按空白分词逐词处理，保留原始空白；仅掩秘钥 token 本体（含首尾
/// 引号/括号等标点也保留），命令结构保持可审阅（权限弹窗用户
/// 仍能看懂将执行什么）。权限弹窗 command 字段经
/// [`super::permission_bridge::to_view`] 同样走本函数。
pub fn mask_embedded_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut expect_bearer_token = false;
    let mut rest = text;
    while !rest.is_empty() {
        // 空白段原样保留
        let ws_end = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        out.push_str(&rest[..ws_end]);
        rest = &rest[ws_end..];
        if rest.is_empty() {
            break;
        }
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        rest = &rest[word_end..];
        if expect_bearer_token && mask_bearer_token(&mut out, word) {
            // token 本体已掩，首尾标点已保留
        } else {
            out.push_str(&mask_secret_word(word));
        }
        expect_bearer_token = is_bearer_keyword(word);
    }
    out
}

/// 词尾为 `bearer`（大小写不敏感）且前一字符非字母数字时视为 Bearer
/// 关键词：覆盖 `Authorization:Bearer x` / `Authorization:"Bearer x"` 等
/// 无空白分隔形态（否则 token 明文外泄）；`cupbearer` 等普通词尾
/// 不误判。
fn is_bearer_keyword(word: &str) -> bool {
    let bytes = word.as_bytes();
    let Some(start) = bytes.len().checked_sub(6) else {
        return false;
    };
    if !bytes[start..].eq_ignore_ascii_case(b"bearer") {
        return false;
    }
    start == 0 || !bytes[start - 1].is_ascii_alphanumeric()
}

/// Bearer 后继词掩码：仅掩词内首个 token 连续段（≥4 位，字符集含
/// JWT/base64 常用的 `.+/=`），首尾标点（引号/括号等）原样保留。
/// 命中并写入 `out` 时返回 true；无可掩段时返回 false（回退常规处理）。
fn mask_bearer_token(out: &mut String, word: &str) -> bool {
    let is_token_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '=');
    let Some(start) = word.find(is_token_char) else {
        return false;
    };
    let end = word[start..]
        .find(|c: char| !is_token_char(c))
        .map(|i| start + i)
        .unwrap_or(word.len());
    if end - start < 4 {
        return false;
    }
    out.push_str(&word[..start]);
    out.push_str("***");
    out.push_str(&word[end..]);
    true
}

/// 单词级掩码：词内 Bearer 形态、URL userinfo 与 sk- 前缀长密钥。
fn mask_secret_word(word: &str) -> String {
    // 词内 Bearer 形态：关键词与 token 同词、经非字母数字分隔
    //（如 `Authorization:Bearer:x`、`Bearer=x`，跨词逻辑覆盖不到）
    if let Some(masked) = mask_inword_bearer(word) {
        return masked;
    }
    // URL userinfo：authority 段（至首个 /?# 或词尾）含 `@` 时掩去凭据
    if let Some(scheme_end) = word.find("://") {
        let auth_start = scheme_end + 3;
        let auth_end = word[auth_start..]
            .find(['/', '?', '#'])
            .map(|i| auth_start + i)
            .unwrap_or(word.len());
        if let Some(at) = word[auth_start..auth_end].rfind('@') {
            return format!("{}***{}", &word[..auth_start], &word[auth_start + at..]);
        }
    }
    // sk- 前缀长密钥：词内最大 [A-Za-z0-9_-] 连续段 ≥20 位且以 sk- 开头
    const MIN_KEY_RUN: usize = 20;
    let is_key_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    let bytes = word.as_bytes();
    let mut out = String::with_capacity(word.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii() && is_key_byte(bytes[i]) {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii() && is_key_byte(bytes[i]) {
                i += 1;
            }
            let run = &word[start..i];
            if run.len() >= MIN_KEY_RUN && run.starts_with("sk-") {
                out.push_str("***");
            } else {
                out.push_str(run);
            }
        } else {
            // 非 key 字符（含多字节 UTF-8）按 char 前进
            let ch_len = word[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&word[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

/// 词内 `bearer` 子串（大小写不敏感、前边界非字母数字）后紧跟非
/// 字母数字分隔符与 token 段时，掩去该 token 段（复用
/// [`mask_bearer_token`] 的 ≥4 位规则与标点保留）。词尾 bearer
///（token 在下一词）由跨词逻辑处理，本函数返回 `None`。
fn mask_inword_bearer(word: &str) -> Option<String> {
    // "bearer" 为纯 ASCII，to_ascii_lowercase 保持字节对齐，索引可直接
    // 映回原词（多字节字符不受影响）
    let lower = word.to_ascii_lowercase();
    let mut search = 0;
    while let Some(rel) = lower[search..].find("bearer") {
        let start = search + rel;
        search = start + 6;
        // 前边界：词首或非字母数字（cupbearer 等普通词不误判）
        if start > 0 && lower.as_bytes()[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let rest = &word[start + 6..];
        // 后继须以非字母数字分隔（bearers 等不误判；词尾 bearer
        // 交给跨词逻辑），且需存在可掩 token 段
        match rest.chars().next() {
            Some(c) if c.is_ascii_alphanumeric() => continue,
            None => continue,
            _ => {}
        }
        let mut out = String::with_capacity(word.len());
        out.push_str(&word[..start + 6]);
        if mask_bearer_token(&mut out, rest) {
            return Some(out);
        }
    }
    None
}

/// 将用户输入落入视图状态（发送前调用）。
pub fn push_user(state: &mut ChatViewState, text: &str) {
    state.push_item(ChatItem::Text {
        role: ChatRole::User,
        text: text.to_string(),
    });
    state.busy = true;
}

/// 标记已向 Kernel 请求中断。
///
/// 中断确认到达前，宿主必须通过 [`can_send`] 保持输入区不可发送。这样旧
/// 查询的 `QUERY_INTERRUPTED_STATUS` 不会在新查询已开始后错误复位其 busy 位。
pub fn request_interrupt(state: &mut ChatViewState) {
    state.interrupt_pending = true;
    state.busy = false;
}

/// 当前是否可以开始新的用户查询。
pub fn can_send(state: &ChatViewState) -> bool {
    !state.interrupt_pending
}

/// 发送失败回退：从尾部移除最近一条文本完全匹配的 user 条目，
/// 使可见转写与镜像/持久历史保持一致（内核从未收到该消息）。
/// 从尾部匹配而非直接 pop：防御 push 与失败之间流事件插入新条目。
pub fn retract_last_user(state: &mut ChatViewState, text: &str) -> bool {
    if let Some(pos) = state.items.iter().rposition(
        |item| matches!(item, ChatItem::Text { role: ChatRole::User, text: t } if t == text),
    ) {
        state.remove_item_at(pos);
        return true;
    }
    false
}

/// 应用单个 `StreamEvent`。返回 `AssistantTurnComplete` 携带的完整消息
/// （宿主用于会话镜像持久化），其余事件返回 `None`。
pub fn apply_stream_event(
    state: &mut ChatViewState,
    event: StreamEvent,
) -> Option<ConversationMessage> {
    match event {
        StreamEvent::AssistantTextDelta { text } => {
            state.streaming_text.push_str(&text);
            None
        }
        StreamEvent::AssistantTurnComplete { message, .. } => {
            // 流式尾部落定为正式条目（以完整消息文本为准，抗 delta 丢失）
            state.streaming_text.clear();
            let text = message.text();
            if !text.trim().is_empty() {
                state.push_item(ChatItem::Text {
                    role: ChatRole::Assistant,
                    text,
                });
            }
            // turn 结束不等于查询结束（可能继续工具轮）；busy 由 Error/
            // 空闲侧翻转，这里保持 true，待宿主在无后续事件时复位。
            Some(message)
        }
        StreamEvent::ToolExecutionStarted {
            tool_use_id,
            tool_name,
            tool_input,
        } => {
            state.push_item(ChatItem::ToolCall {
                tool_use_id,
                name: tool_name,
                input_preview: pretty_json(&mask_sensitive(tool_input)),
                status: ToolCallStatus::Running,
                output_preview: String::new(),
            });
            None
        }
        StreamEvent::ToolExecutionCompleted {
            tool_use_id,
            output,
            is_error,
            ..
        } => {
            // 按 `tool_use` 协议 id 精确落定对应 Running 卡片
            //（同名工具不误配；找不到则忽略，乱序完成容错）
            if let Some(ChatItem::ToolCall {
                status,
                output_preview,
                ..
            }) = state.items.iter_mut().find(|item| {
                matches!(
                    item,
                    ChatItem::ToolCall { tool_use_id: id, status, .. }
                        if *status == ToolCallStatus::Running && *id == tool_use_id
                )
            }) {
                *status = if is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Done
                };
                *output_preview = output;
            }
            None
        }
        StreamEvent::Error {
            message,
            recoverable,
        } => {
            state.streaming_text.clear();
            state.busy = false;
            // Kernel 异常结束时不会再有中断确认；允许用户重新发送。
            state.interrupt_pending = false;
            state.push_item(ChatItem::ErrorNote {
                text: message,
                recoverable,
            });
            None
        }
        StreamEvent::Status { message } => {
            // The kernel emits this status after it has stopped the active query.
            // Discard partial output so a later query cannot append to the
            // interrupted response.
            if message == QUERY_INTERRUPTED_STATUS {
                state.streaming_text.clear();
                state.busy = false;
                state.interrupt_pending = false;
            }
            state.push_item(ChatItem::StatusNote { text: message });
            None
        }
        StreamEvent::CompactProgress { phase, .. } => {
            state.push_item(ChatItem::CompactNote { phase });
            None
        }
        // 压缩完成事件（§11.1）：进度已由 CompactProgress 展示，本分支仅
        // 保持 exhaustive match（宿主侧负责 ordered checkpoint + extraction）。
        StreamEvent::Compacted { .. } => None,
    }
}

/// 一轮查询自然结束（宿主检测到流暂无后续且无 Running 工具）时复位忙碌位。
///
/// 无工具的 `AssistantTurnComplete` 是 Kernel 已自然结束该查询的确认。若用户
/// 恰在该确认到达 UI 前点击 Stop，`interrupt_pending` 是陈旧状态：保留它会
/// 锁住输入框，且下一次查询可能被遗留的原子中断标志取消。因此自然结束同样
/// 必须解除 UI 侧的中断等待；宿主同时负责清除对应的原子标志。
pub fn settle_idle(state: &mut ChatViewState) {
    let tool_running = state.items.iter().any(|item| {
        matches!(
            item,
            ChatItem::ToolCall {
                status: ToolCallStatus::Running,
                ..
            }
        )
    });
    if !tool_running && state.streaming_text.is_empty() {
        state.busy = false;
        state.interrupt_pending = false;
    }
}

/// 会话镜像：忠实重建 Kernel 内部对话（含合成的 tool_result 消息），
/// 作为会话快照持久化源。
///
/// 关键：Kernel 在工具轮后会追加 `tool_result` 用户消息，若快照缺失该消息，
/// `sanitize_conversation_messages` 会把带未配对 `tool_use` 的 assistant 消息
/// 整条丢弃（含其文本），导致中间工具轮在重启后丢失。本镜像据流事件补齐
/// `tool_result` 消息，与 Kernel 追加顺序一致（按 `tool_use` 出现序）。
pub struct ConversationMirror {
    messages: Vec<ConversationMessage>,
    /// 当前 assistant 轮待完成的 tool_use（按出现序）。
    pending: Vec<ToolUse>,
    /// 与 `pending` 平行的结果槽（None 表示未完成）。
    results: Vec<Option<(String, bool)>>,
}

impl ConversationMirror {
    /// 由恢复的历史种子构造（历史内的 tool_result 已在其中，无需补齐）。
    pub fn new(seed: Vec<ConversationMessage>) -> Self {
        Self {
            messages: seed,
            pending: Vec::new(),
            results: Vec::new(),
        }
    }

    /// 追加用户文本。若尚有未完成的 tool_use 收集窗口（中断按钮仅复位
    /// UI 忙碌位，输入框可能在工具运行中重新可用），先以占位结果
    /// （`is_error=true`、"interrupted"）关闭窗口再追加：保证快照中
    /// tool_result 紧跟其 tool_use（sanitize 要求），避免插入的用户消息
    /// 导致整个工具轮在恢复时被丢弃。
    pub fn push_user_text(&mut self, text: &str) {
        if !self.pending.is_empty() {
            self.flush_pending_as_interrupted();
        }
        self.messages
            .push(ConversationMessage::from_user_text(text));
    }

    /// 将未完成的 tool_use 以中断占位结果落定，追加 tool_result 用户
    /// 消息并关闭收集窗口（已完成槽位保留真实结果）。
    fn flush_pending_as_interrupted(&mut self) {
        let blocks = self
            .pending
            .iter()
            .zip(self.results.iter())
            .map(|(tool_use, result)| {
                let (content, is_error) = result
                    .clone()
                    .unwrap_or_else(|| ("interrupted".to_string(), true));
                ContentBlock::ToolResult {
                    tool_use_id: tool_use.id.clone(),
                    content,
                    is_error,
                    result_metadata: Value::Null,
                }
            })
            .collect();
        self.messages
            .push(ConversationMessage::from_user_content(blocks));
        self.pending.clear();
        self.results.clear();
    }

    /// 记录一个 assistant turn（完整消息按原样入镜像）；若含 tool_use 则
    /// 开启等待其 tool_result 的收集窗口。
    pub fn on_turn_complete(&mut self, message: ConversationMessage) {
        let tool_uses = message.tool_uses();
        self.messages.push(message);
        self.results = vec![None; tool_uses.len()];
        self.pending = tool_uses;
    }

    /// 记录一次工具完成（按 `tool_use` 协议 id 与待完成槽位精确配对，
    /// 同名工具不误配）。当本轮全部 tool_use 均有结果时，追加一条含
    /// 全部 tool_result block 的用户消息并返回 `true`（宿主据此触发持久化）。
    pub fn on_tool_completed(&mut self, tool_use_id: &str, output: String, is_error: bool) -> bool {
        if let Some(idx) = (0..self.pending.len())
            .find(|&i| self.pending[i].id == tool_use_id && self.results[i].is_none())
        {
            self.results[idx] = Some((output, is_error));
        }
        if !self.pending.is_empty() && self.results.iter().all(Option::is_some) {
            let blocks = self
                .pending
                .iter()
                .zip(self.results.iter())
                .map(|(tool_use, result)| {
                    let (content, is_error) = result.clone().unwrap_or_default();
                    ContentBlock::ToolResult {
                        tool_use_id: tool_use.id.clone(),
                        content,
                        is_error,
                        result_metadata: Value::Null,
                    }
                })
                .collect();
            self.messages
                .push(ConversationMessage::from_user_content(blocks));
            self.pending.clear();
            self.results.clear();
            return true;
        }
        false
    }

    pub fn snapshot(&self) -> Vec<ConversationMessage> {
        self.messages.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::model_client::UsageSnapshot;
    use serde_json::json;

    fn assistant_msg(text: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn turn_complete(text: &str) -> StreamEvent {
        StreamEvent::AssistantTurnComplete {
            message: assistant_msg(text),
            usage: UsageSnapshot::default(),
            tool_metadata: Default::default(),
        }
    }

    #[test]
    fn delta_accumulates_then_turn_complete_settles() {
        let mut state = ChatViewState::default();
        push_user(&mut state, "hi");
        assert!(state.busy);

        apply_stream_event(
            &mut state,
            StreamEvent::AssistantTextDelta { text: "Hel".into() },
        );
        apply_stream_event(
            &mut state,
            StreamEvent::AssistantTextDelta { text: "lo".into() },
        );
        assert_eq!(state.streaming_text, "Hello");

        let mirrored = apply_stream_event(&mut state, turn_complete("Hello"));
        assert!(mirrored.is_some());
        assert!(state.streaming_text.is_empty());
        assert_eq!(state.items.len(), 2);
        assert_eq!(
            state.items[1],
            ChatItem::Text {
                role: ChatRole::Assistant,
                text: "Hello".into()
            }
        );
    }

    #[test]
    fn empty_turn_complete_produces_no_item_but_returns_mirror() {
        let mut state = ChatViewState::default();
        let mirrored = apply_stream_event(&mut state, turn_complete("   "));
        assert!(mirrored.is_some());
        assert!(state.items.is_empty());
    }

    #[test]
    fn tool_started_creates_running_card() {
        let mut state = ChatViewState::default();
        apply_stream_event(
            &mut state,
            StreamEvent::ToolExecutionStarted {
                tool_use_id: "t1".into(),
                tool_name: "calculator".into(),
                tool_input: json!({"expression": "1+1"}),
            },
        );
        assert!(matches!(
            &state.items[0],
            ChatItem::ToolCall { name, status: ToolCallStatus::Running, .. } if name == "calculator"
        ));
    }

    #[test]
    fn tool_completed_pairs_by_tool_use_id_not_name_order() {
        // 同名工具两张 Running 卡片：完成事件按协议 id 精确落定
        // 第二张（若按名称 FIFO 会误配第一张）
        let mut state = ChatViewState::default();
        for id in ["t1", "t2"] {
            apply_stream_event(
                &mut state,
                StreamEvent::ToolExecutionStarted {
                    tool_use_id: id.into(),
                    tool_name: "glob".into(),
                    tool_input: json!({}),
                },
            );
        }
        apply_stream_event(
            &mut state,
            StreamEvent::ToolExecutionCompleted {
                tool_use_id: "t2".into(),
                tool_name: "glob".into(),
                output: "12 files".into(),
                is_error: false,
                metadata: json!({}),
                tool_metadata: Default::default(),
            },
        );
        let statuses: Vec<_> = state
            .items
            .iter()
            .map(|item| match item {
                ChatItem::ToolCall { status, .. } => *status,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(statuses, [ToolCallStatus::Running, ToolCallStatus::Done]);
    }

    #[test]
    fn tool_completed_error_marks_failed_card() {
        let mut state = ChatViewState::default();
        apply_stream_event(
            &mut state,
            StreamEvent::ToolExecutionStarted {
                tool_use_id: "t1".into(),
                tool_name: "web_fetch".into(),
                tool_input: json!({"url": "https://x"}),
            },
        );
        apply_stream_event(
            &mut state,
            StreamEvent::ToolExecutionCompleted {
                tool_use_id: "t1".into(),
                tool_name: "web_fetch".into(),
                output: "denied".into(),
                is_error: true,
                metadata: json!({}),
                tool_metadata: Default::default(),
            },
        );
        assert!(matches!(
            &state.items[0],
            ChatItem::ToolCall { status: ToolCallStatus::Failed, output_preview, .. }
                if output_preview == "denied"
        ));
    }

    #[test]
    fn unmatched_tool_completed_is_ignored() {
        let mut state = ChatViewState::default();
        apply_stream_event(
            &mut state,
            StreamEvent::ToolExecutionCompleted {
                tool_use_id: "ghost".into(),
                tool_name: "phantom".into(),
                output: "x".into(),
                is_error: false,
                metadata: json!({}),
                tool_metadata: Default::default(),
            },
        );
        assert!(state.items.is_empty());
    }

    #[test]
    fn unrecoverable_error_clears_busy_and_streaming() {
        let mut state = ChatViewState::default();
        push_user(&mut state, "go");
        apply_stream_event(
            &mut state,
            StreamEvent::AssistantTextDelta { text: "par".into() },
        );
        apply_stream_event(
            &mut state,
            StreamEvent::Error {
                message: "boom".into(),
                recoverable: false,
            },
        );
        assert!(!state.busy);
        assert!(state.streaming_text.is_empty());
        assert!(matches!(
            state.items.last().unwrap(),
            ChatItem::ErrorNote {
                recoverable: false,
                ..
            }
        ));
    }

    #[test]
    fn recoverable_error_is_labelled() {
        let mut state = ChatViewState::default();
        apply_stream_event(
            &mut state,
            StreamEvent::Error {
                message: "retry later".into(),
                recoverable: true,
            },
        );
        assert!(matches!(
            state.items.last().unwrap(),
            ChatItem::ErrorNote {
                recoverable: true,
                ..
            }
        ));
    }

    #[test]
    fn status_and_compact_progress_become_notes() {
        let mut state = ChatViewState::default();
        state.streaming_text = "partial response".into();
        apply_stream_event(
            &mut state,
            StreamEvent::Status {
                message: "retrying".into(),
            },
        );
        apply_stream_event(
            &mut state,
            StreamEvent::CompactProgress {
                phase: "microcompact".into(),
                trigger: agent_core::kernel::CompactTrigger::Auto,
            },
        );
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.streaming_text, "partial response");
        assert!(matches!(&state.items[0], ChatItem::StatusNote { text } if text == "retrying"));
        assert!(
            matches!(&state.items[1], ChatItem::CompactNote { phase } if phase == "microcompact")
        );
    }

    #[test]
    fn interruption_status_discards_partial_stream_before_next_delta() {
        let mut state = ChatViewState::default();
        state.busy = true;

        apply_stream_event(
            &mut state,
            StreamEvent::AssistantTextDelta {
                text: "partial response".into(),
            },
        );
        apply_stream_event(
            &mut state,
            StreamEvent::Status {
                message: QUERY_INTERRUPTED_STATUS.into(),
            },
        );

        assert!(state.streaming_text.is_empty());
        assert!(!state.busy);
        assert!(matches!(
            state.items.last(),
            Some(ChatItem::StatusNote { text }) if text == QUERY_INTERRUPTED_STATUS
        ));

        apply_stream_event(
            &mut state,
            StreamEvent::AssistantTextDelta {
                text: "new response".into(),
            },
        );
        assert_eq!(state.streaming_text, "new response");
    }

    #[test]
    fn interruption_acknowledgement_gates_next_send() {
        let mut state = ChatViewState::default();
        push_user(&mut state, "first query");

        request_interrupt(&mut state);
        assert!(!state.busy);
        assert!(!can_send(&state));

        // The host keeps the draft intact rather than starting a second query
        // whose busy state could be cleared by this delayed status event.
        apply_stream_event(
            &mut state,
            StreamEvent::Status {
                message: QUERY_INTERRUPTED_STATUS.into(),
            },
        );

        assert!(can_send(&state));
        push_user(&mut state, "second query");
        assert!(state.busy);
        assert!(matches!(
            state.items.last(),
            Some(ChatItem::Text { role: ChatRole::User, text }) if text == "second query"
        ));
    }

    #[test]
    fn final_turn_acknowledges_a_stop_that_arrived_too_late() {
        // Kernel may complete a no-tool turn immediately before the UI handles
        // Stop. That is a natural completion, not an in-flight cancellation;
        // leaving interrupt_pending set would disable every later send.
        let mut state = ChatViewState::default();
        push_user(&mut state, "first query");
        request_interrupt(&mut state);

        apply_stream_event(
            &mut state,
            StreamEvent::AssistantTurnComplete {
                message: ConversationMessage {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "completed".into(),
                    }],
                },
                usage: Default::default(),
                tool_metadata: Default::default(),
            },
        );
        settle_idle(&mut state);

        assert!(!state.busy);
        assert!(
            can_send(&state),
            "a completed turn must release the composer"
        );
    }

    #[test]
    fn settle_idle_only_when_no_running_tools() {
        let mut state = ChatViewState::default();
        push_user(&mut state, "go");
        apply_stream_event(
            &mut state,
            StreamEvent::ToolExecutionStarted {
                tool_use_id: "t1".into(),
                tool_name: "date".into(),
                tool_input: json!({}),
            },
        );
        settle_idle(&mut state);
        assert!(state.busy, "running tool keeps busy");

        apply_stream_event(
            &mut state,
            StreamEvent::ToolExecutionCompleted {
                tool_use_id: "t1".into(),
                tool_name: "date".into(),
                output: "2026".into(),
                is_error: false,
                metadata: json!({}),
                tool_metadata: Default::default(),
            },
        );
        settle_idle(&mut state);
        assert!(!state.busy);
    }

    #[test]
    fn seed_history_maps_text_and_paired_tool_blocks() {
        let messages = vec![
            ConversationMessage::from_user_text("list files"),
            ConversationMessage {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text { text: "ok".into() },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "glob".into(),
                        input: json!({"pattern": "*.rs"}),
                    },
                ],
            },
            ConversationMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "3 files".into(),
                    is_error: false,
                    result_metadata: json!({}),
                }],
            },
        ];
        let items = seed_history(&messages);
        assert_eq!(items.len(), 3);
        assert!(matches!(
            &items[0],
            ChatItem::Text {
                role: ChatRole::User,
                ..
            }
        ));
        assert!(matches!(
            &items[2],
            ChatItem::ToolCall { status: ToolCallStatus::Done, output_preview, .. }
                if output_preview == "3 files"
        ));
    }

    #[test]
    fn seed_history_renders_dangling_tool_use_as_failed() {
        // 无配对 tool_result 的 tool_use（历史中断残留）按失败展示，
        // 而非假报 Done 空输出
        let items = seed_history(&[tool_use_msg("t1", "glob")]);
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            ChatItem::ToolCall { status: ToolCallStatus::Failed, output_preview, .. }
                if output_preview.is_empty()
        ));
    }

    #[test]
    fn seed_history_renders_expanded_skill_prompt_as_alias() {
        // /skill 展开指令恢复时渲染为 `/skill {name}` 别名，
        // 不将整段 skill 正文倾泻进可见历史（与实时转写一致）
        let prompt = skill_prompt("csv-report", "# Big Body\nstep 1\nstep 2");
        let items = seed_history(&[ConversationMessage::from_user_text(&prompt)]);
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            ChatItem::Text { role: ChatRole::User, text } if text == "/skill csv-report"
        ));
        // 普通用户文本不受影响；assistant 角色不误缩写
        let items = seed_history(&[
            ConversationMessage::from_user_text("just a normal question"),
            assistant_msg(&prompt),
        ]);
        assert!(matches!(
            &items[0],
            ChatItem::Text { text, .. } if text == "just a normal question"
        ));
        assert!(matches!(&items[1], ChatItem::Text { text, .. } if *text == prompt));
    }

    #[test]
    fn skill_display_alias_only_matches_wellformed_expansion() {
        assert_eq!(
            skill_display_alias(&skill_prompt("x", "body")).as_deref(),
            Some("/skill x")
        );
        // 不完整 / 非展开指令返回 None
        assert_eq!(skill_display_alias("Apply the skill `x`"), None);
        assert_eq!(skill_display_alias("hello world"), None);
        assert_eq!(skill_display_alias(&skill_prompt("", "body")), None);
    }

    #[test]
    fn mask_sensitive_masks_matching_keys_recursively() {
        let masked = mask_sensitive(json!({
            "url": "https://x",
            "api_key": "sk-live-123",
            "headers": {"Authorization": "Bearer abc", "Accept": "json"},
            "items": [{"password": "p", "name": "n"}],
        }));
        assert_eq!(masked["url"], "https://x");
        assert_eq!(masked["api_key"], "***");
        assert_eq!(masked["headers"]["Authorization"], "***");
        assert_eq!(masked["headers"]["Accept"], "json");
        assert_eq!(masked["items"][0]["password"], "***");
        assert_eq!(masked["items"][0]["name"], "n");
    }

    #[test]
    fn mask_sensitive_is_case_insensitive_and_substring() {
        let masked = mask_sensitive(json!({
            "AccessToken": "t",
            "user_passwd": "p",
            "ApiKey": "k",
            "plain": 1,
        }));
        assert_eq!(masked["AccessToken"], "***");
        assert_eq!(masked["user_passwd"], "***");
        assert_eq!(masked["ApiKey"], "***");
        assert_eq!(masked["plain"], 1);
    }

    #[test]
    fn mask_sensitive_masks_bearer_tokens_embedded_in_string_values() {
        // 非敏感 key（command）的值内嵌 Bearer 秘钥 → 仅掩 token 本体，
        // 命令结构保持可审阅
        let masked = mask_sensitive(json!({
            "command": "curl -H 'Authorization: Bearer abc123secret' https://api.example.com/v1",
        }));
        let text = masked["command"].as_str().unwrap();
        assert!(!text.contains("abc123secret"), "token 本体必须被掩: {text}");
        assert!(text.contains("Bearer ***'"), "尾部引号保留: {text}");
        assert!(text.contains("curl -H"), "命令结构保留: {text}");
        assert!(
            text.contains("https://api.example.com/v1"),
            "无凭据 URL 不受影响"
        );

        // 关键词大小写不敏感；短词（<4 字符，如自然语言 "to"）不误掩
        let masked = mask_sensitive(json!({
            "a": "use bearer XYZ9token now",
            "b": "Bearer to auth",
        }));
        assert_eq!(masked["a"], "use bearer *** now");
        assert_eq!(masked["b"], "Bearer to auth");
    }

    #[test]
    fn mask_sensitive_masks_bearer_without_whitespace_separator() {
        // 词边界绕过回归：冒号/引号紧贴 Bearer（无空白分隔）时
        // token 仍须被掩
        let masked = mask_sensitive(json!({
            "a": r#"curl -H Authorization:"Bearer s3cret42" https://api"#,
            "b": "curl -H Authorization:Bearer s3cret42 https://api",
        }));
        for key in ["a", "b"] {
            let text = masked[key].as_str().unwrap();
            assert!(!text.contains("s3cret42"), "token 必须被掩: {text}");
            assert!(text.contains("***"), "应含掩码占位: {text}");
            assert!(text.contains("https://api"), "命令结构保留: {text}");
        }
        // 普通词尾 bearer（字母数字前缀）不误判为关键词
        let masked = mask_sensitive(json!({
            "c": "the cupbearer wine list",
        }));
        assert_eq!(masked["c"], "the cupbearer wine list");
    }

    #[test]
    fn mask_sensitive_masks_inword_bearer_with_separator() {
        // 词内形态回归：关键词与 token 同词、经非字母数字分隔
        //（跨词逻辑覆盖不到）时 token 仍须被掩
        let masked = mask_sensitive(json!({
            "a": "Authorization:Bearer:s3cret42",
            "b": "Bearer=s3cret42",
            "c": "header=bearer.s3cret42;next",
        }));
        for key in ["a", "b", "c"] {
            let text = masked[key].as_str().unwrap();
            assert!(!text.contains("s3cret42"), "token 必须被掩: {key}={text}");
            assert!(text.contains("***"), "应含掩码占位: {key}={text}");
        }
        // 非 token 字符分隔（':'）保留；token 字符分隔（'='/'.'）
        // 则并入掩码段（安全无损，仅形状差异），未掩结构保留
        assert_eq!(masked["a"], "Authorization:Bearer:***");
        assert_eq!(masked["b"], "Bearer***");
        assert_eq!(masked["c"], "header=bearer***;next");

        // bearers / cupbearer 等不误判（后继为字母 / 前边界为字母）
        let masked = mask_sensitive(json!({
            "d": "bearers12345678",
            "e": "cupbearer=x1234",
        }));
        assert_eq!(masked["d"], "bearers12345678");
        assert_eq!(masked["e"], "cupbearer=x1234");
    }

    #[test]
    fn mask_sensitive_masks_url_userinfo_and_sk_keys_in_string_values() {
        let masked = mask_sensitive(json!({
            "url": "https://alice:hunter2@example.com/path?q=1",
            "api": "use sk-live-aaaaaaaaaaaaaaaaaaaaaaaa for calls",
            "plain": "no secrets here sk-short user@example.com",
        }));
        // URL userinfo 掩去凭据，保留 host 与路径
        assert_eq!(masked["url"], "https://***@example.com/path?q=1");
        // sk- 前缀长密钥整段掩去
        assert_eq!(masked["api"], "use *** for calls");
        // 短 sk- 与无 scheme 的邮箱不误掩
        assert_eq!(masked["plain"], "no secrets here sk-short user@example.com");
    }

    #[test]
    fn mask_embedded_secrets_handles_multibyte_adjacency() {
        // 多字节字符紧邻可掩段：不得 panic（char 边界切片）且掩码正确
        let masked = mask_sensitive(json!({
            "a": "密钥sk-aaaaaaaaaaaaaaaaaaaaaa结束",
            "b": "参见https://user:p@host/路径",
            "c": "令牌 Bearer t0ken1234（括号）",
        }));
        assert_eq!(masked["a"], "密钥***结束");
        assert_eq!(masked["b"], "参见https://***@host/路径");
        // 全角括号（非 token 字符）紧跟 token 本体：仅掩 token 段
        assert_eq!(masked["c"], "令牌 Bearer ***（括号）");
    }

    #[test]
    fn retract_last_user_removes_matching_tail_entry_only() {
        // 发送失败回收：移除尾部匹配条目，不误伤同文本的早期条目与
        // push 之后插入的流事件条目
        let mut state = ChatViewState::default();
        push_user(&mut state, "hi");
        apply_stream_event(&mut state, turn_complete("hello"));
        push_user(&mut state, "hi"); // 第二次发送同文本 → 失败
        state.items.push(ChatItem::StatusNote {
            text: "late event".into(),
        }); // push 与失败之间插入的流事件

        assert!(retract_last_user(&mut state, "hi"));
        // 首条 user + assistant + 晚到的 StatusNote 保留；仅尾部匹配被移除
        assert_eq!(state.items.len(), 3);
        assert!(matches!(
            &state.items[0],
            ChatItem::Text { role: ChatRole::User, text } if text == "hi"
        ));
        assert!(matches!(&state.items[2], ChatItem::StatusNote { .. }));
        // 无匹配时返回 false 且不改变状态
        assert!(!retract_last_user(&mut state, "never sent"));
        assert_eq!(state.items.len(), 3);
    }

    #[test]
    fn tool_started_masks_sensitive_input_in_card() {
        let mut state = ChatViewState::default();
        apply_stream_event(
            &mut state,
            StreamEvent::ToolExecutionStarted {
                tool_use_id: "t1".into(),
                tool_name: "web_fetch".into(),
                tool_input: json!({"url": "https://x", "authorization": "Bearer secret"}),
            },
        );
        match &state.items[0] {
            ChatItem::ToolCall { input_preview, .. } => {
                assert!(input_preview.contains("https://x"));
                assert!(input_preview.contains("***"));
                assert!(!input_preview.contains("Bearer secret"));
            }
            _ => panic!("expected ToolCall"),
        }
    }

    fn tool_use_msg(id: &str, name: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: json!({}),
            }],
        }
    }

    #[test]
    fn mirror_appends_tool_result_message_so_sanitize_keeps_tool_turn() {
        use agent_core::kernel::sanitize_conversation_messages;
        let mut mirror = ConversationMirror::new(vec![]);
        mirror.push_user_text("run glob");
        mirror.on_turn_complete(tool_use_msg("t1", "glob"));
        // 未配对前镜像仅含 user + assistant(tool_use)，尚无 tool_result
        assert_eq!(
            mirror.snapshot().len(),
            2,
            "pending tool keeps query active"
        );
        // 未配对前 sanitize 会丢弃 assistant tool_use 轮
        assert_eq!(
            sanitize_conversation_messages(mirror.snapshot()).len(),
            1,
            "unpaired tool_use turn is dropped"
        );

        let persisted = mirror.on_tool_completed("t1", "3 files".into(), false);
        assert!(persisted, "all tools done → persist");
        let snapshot = mirror.snapshot();
        assert_eq!(
            snapshot.len(),
            3,
            "user + assistant(tool_use) + tool_result"
        );
        // 补齐 tool_result 后 sanitize 保留完整工具轮
        assert_eq!(sanitize_conversation_messages(snapshot).len(), 3);
    }

    #[test]
    fn mirror_pairs_same_name_tools_by_id_and_preserves_tool_use_order() {
        let mut mirror = ConversationMirror::new(vec![]);
        let assistant = ConversationMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "glob".into(),
                    input: json!({}),
                },
                ContentBlock::ToolUse {
                    id: "b".into(),
                    name: "glob".into(),
                    input: json!({}),
                },
            ],
        };
        mirror.on_turn_complete(assistant);
        // 乱序完成（先 b 后 a）：按 id 精确落槽，同名不误配
        assert!(!mirror.on_tool_completed("b", "second".into(), false));
        assert!(mirror.on_tool_completed("a", "first".into(), false));

        let last = mirror.snapshot().pop().unwrap();
        match (&last.content[0], &last.content[1]) {
            (
                ContentBlock::ToolResult {
                    tool_use_id: id0,
                    content: c0,
                    ..
                },
                ContentBlock::ToolResult {
                    tool_use_id: id1,
                    content: c1,
                    ..
                },
            ) => {
                assert_eq!((id0.as_str(), c0.as_str()), ("a", "first"));
                assert_eq!((id1.as_str(), c1.as_str()), ("b", "second"));
            }
            _ => panic!("expected two tool_result blocks in tool_use order"),
        }
    }

    #[test]
    fn mirror_final_turn_without_tools_is_idle_immediately() {
        let mut mirror = ConversationMirror::new(vec![]);
        mirror.push_user_text("hi");
        mirror.on_turn_complete(assistant_msg("hello"));
        assert_eq!(mirror.snapshot().len(), 2);
        // 无待完成工具：多余的工具完成事件不会追加任何消息
        assert!(!mirror.on_tool_completed("whatever", "x".into(), false));
        assert_eq!(mirror.snapshot().len(), 2);
    }

    #[test]
    fn mirror_user_text_during_open_tool_window_flushes_interrupted_results() {
        // 回归：中断后用户在工具窗口未关闭时插入消息，快照不得丢失
        // 工具轮（tool_result 必须紧跟 tool_use，否则 sanitize 丢弃整轮）
        use agent_core::kernel::sanitize_conversation_messages;
        let mut mirror = ConversationMirror::new(vec![]);
        mirror.push_user_text("run glob");
        mirror.on_turn_complete(tool_use_msg("t1", "glob"));
        mirror.push_user_text("interleaved");

        let snapshot = mirror.snapshot();
        // user + assistant(tool_use) + 合成 tool_result + 插入的 user
        assert_eq!(snapshot.len(), 4);
        match &snapshot[2].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(content, "interrupted");
                assert!(is_error, "占位结果必须标记为错误");
            }
            other => panic!("expected synthesized tool_result, got {other:?}"),
        }
        // sanitize 保留完整工具轮与插入的用户消息
        assert_eq!(sanitize_conversation_messages(snapshot).len(), 4);

        // 窗口已关闭：迟到的工具完成事件被忽略，不重复追加 tool_result
        assert!(!mirror.on_tool_completed("t1", "late".into(), false));
        assert_eq!(mirror.snapshot().len(), 4);
    }

    #[test]
    fn mirror_interrupt_flush_keeps_already_completed_results() {
        // 部分完成后中断：已完成槽位保留真实结果，未完成槽位落占位
        let mut mirror = ConversationMirror::new(vec![]);
        let assistant = ConversationMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "glob".into(),
                    input: json!({}),
                },
                ContentBlock::ToolUse {
                    id: "b".into(),
                    name: "read".into(),
                    input: json!({}),
                },
            ],
        };
        mirror.on_turn_complete(assistant);
        assert!(!mirror.on_tool_completed("a", "3 files".into(), false));
        mirror.push_user_text("stop and do something else");

        let snapshot = mirror.snapshot();
        assert_eq!(snapshot.len(), 3, "assistant + tool_result + user");
        match (&snapshot[1].content[0], &snapshot[1].content[1]) {
            (
                ContentBlock::ToolResult {
                    tool_use_id: id0,
                    content: c0,
                    is_error: e0,
                    ..
                },
                ContentBlock::ToolResult {
                    tool_use_id: id1,
                    content: c1,
                    is_error: e1,
                    ..
                },
            ) => {
                assert_eq!((id0.as_str(), c0.as_str(), *e0), ("a", "3 files", false));
                assert_eq!((id1.as_str(), c1.as_str(), *e1), ("b", "interrupted", true));
            }
            other => panic!("expected two tool_result blocks, got {other:?}"),
        }
    }
}
