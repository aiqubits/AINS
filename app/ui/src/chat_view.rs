use dioxus::prelude::*;
use dioxus_icons::lucide::{Check, ChevronDown, ChevronRight, CircleStop, Copy, SendHorizontal};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};

use crate::{EN, I18nContext, tf};

/// 消息角色（视图模型，宿主负责从 rust-agent 映射，见 AINS_PLAN 6.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
}

/// 工具调用卡片状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Done,
    Failed,
}

/// 对话流单个条目（宿主从 `StreamEvent` 映射产出）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    /// 普通文本消息（使用安全的 GitHub Markdown 渲染）。
    Text { role: ChatRole, text: String },
    /// 工具调用卡片：Started 时创建为 Running，Completed 后落定。
    ToolCall {
        /// 配对键（`tool_use` 协议 id，不渲染）：Completed 事件据此
        /// 精确落定对应卡片，同名工具并发时不会错配。
        tool_use_id: String,
        name: String,
        input_preview: String,
        status: ToolCallStatus,
        output_preview: String,
    },
    /// 状态行（重试提示等）。
    StatusNote { text: String },
    /// 压缩进度行。
    CompactNote { phase: String },
    /// 错误行；`recoverable=false` 表示会话终止。
    ErrorNote { text: String, recoverable: bool },
}

/// ChatView 渲染状态：条目列表 + 流式尾部增量 + 忙碌位。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatViewState {
    pub items: Vec<ChatItem>,
    /// 与 `items` 平行的稳定列表 key（单调分配、删除不复用）：
    /// 中段删除（发送失败回收）后行组件本地状态（如工具卡片
    /// 展开位）不随索引错位。请经 [`Self::push_item`] /
    /// [`Self::remove_item_at`] / [`Self::set_items`] 维护同步。
    pub item_keys: Vec<u64>,
    /// 下一个待分配 key（私有：只能经 helper 分配）。
    next_key: u64,
    /// 进行中的 assistant 流式文本（TurnComplete 后清空并落定为 Text 条目）。
    pub streaming_text: String,
    pub busy: bool,
    /// Stop 已发出、但 Kernel 尚未确认停止当前查询。
    ///
    /// 在收到确认前不能开始下一条查询，否则旧查询的中断状态可能在新查询
    /// 已显示为 busy 后到达并错误地将其复位为 idle。
    pub interrupt_pending: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CopyState {
    #[default]
    Idle,
    Copied,
    Failed,
}

/// 将消息文本写入宿主 WebView 的剪贴板。优先使用 Clipboard API，不能使用时
/// 回退到 `execCommand("copy")`，以兼容非安全上下文及部分 Desktop WebView。
fn clipboard_copy_script(text: &str) -> String {
    let encoded = serde_json::to_string(&text).expect("serializing a Rust string cannot fail");
    format!(
        r#"(async () => {{
            const text = {encoded};
            if (typeof navigator !== "undefined" && navigator.clipboard && window.isSecureContext) {{
                try {{
                    await navigator.clipboard.writeText(text);
                    return;
                }} catch (_) {{
                    // Permission may be denied even when the API exists.  Try
                    // the legacy path before reporting a copy failure.
                }}
            }}
            const textarea = document.createElement("textarea");
            textarea.value = text;
            textarea.setAttribute("readonly", "");
            textarea.style.cssText = "position:fixed;left:-9999px;top:0;opacity:0";
            document.body.appendChild(textarea);
            textarea.select();
            const copied = document.execCommand("copy");
            textarea.remove();
            if (!copied) throw new Error("clipboard unavailable");
        }})()"#
    )
}

async fn copy_message_text(text: String) -> bool {
    let script = clipboard_copy_script(&text);
    document::eval(&script).await.is_ok()
}

const CHAT_SCROLL_ID: &str = "ains-chat-scroll";
const CHAT_FOLLOW_THRESHOLD_PX: f64 = 48.0;

/// A small tolerance prevents fractional scroll positions from disabling
/// follow mode while the user is visually at the end of the conversation.
fn is_near_chat_bottom(scroll_top: f64, client_height: i32, scroll_height: i32) -> bool {
    scroll_top + f64::from(client_height) >= f64::from(scroll_height) - CHAT_FOLLOW_THRESHOLD_PX
}

fn scroll_chat_to_bottom() {
    spawn(async move {
        let _ = document::eval(
            "const el = document.getElementById('ains-chat-scroll'); if (el) { el.scrollTop = el.scrollHeight; }",
        )
        .await;
    });
}

impl ChatViewState {
    /// 追加条目并分配稳定 key。
    pub fn push_item(&mut self, item: ChatItem) {
        self.item_keys.push(self.next_key);
        self.next_key += 1;
        self.items.push(item);
    }

    /// 移除指定位置条目（key 同步移除，不复用）。
    pub fn remove_item_at(&mut self, pos: usize) -> ChatItem {
        if pos < self.item_keys.len() {
            self.item_keys.remove(pos);
        }
        self.items.remove(pos)
    }

    /// 整体替换条目（会话恢复种子），全部重新分配 key。
    pub fn set_items(&mut self, items: Vec<ChatItem>) {
        self.item_keys = items
            .iter()
            .map(|_| {
                let key = self.next_key;
                self.next_key += 1;
                key
            })
            .collect();
        self.items = items;
    }
}

/// Chat 对话视图 —— 消息列表 + 流式尾部 + 条件自动滚动（Phase 6.3）。
#[component]
pub fn ChatView(
    state: ReadSignal<ChatViewState>,
    /// Monotonically increasing request from the host when the user has
    /// actively submitted a new prompt. This is intentionally separate from
    /// stream updates so reading history remains stable.
    scroll_to_latest_request: ReadSignal<u64>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    // 用户位于消息末尾时跟随流式输出；主动向上阅读历史则保持阅读位置。
    let mut follow_latest = use_signal(|| true);
    let mut show_jump_to_latest = use_signal(|| false);

    // A successful user submission is an explicit intent to see the new turn.
    // This effect deliberately depends only on the request signal; regular
    // streamed state changes continue through the conditional effect below.
    use_effect(move || {
        let _ = scroll_to_latest_request();
        follow_latest.set(true);
        show_jump_to_latest.set(false);
        scroll_chat_to_bottom();
    });

    // 只在用户仍位于底部时跟随新内容。`peek` 避免用户单纯滚动历史时
    // 触发 effect 并错误显示“回到底部”入口。
    use_effect(move || {
        let snapshot = state.read();
        if snapshot.items.is_empty() && snapshot.streaming_text.is_empty() {
            return;
        }
        if *follow_latest.peek() {
            show_jump_to_latest.set(false);
            scroll_chat_to_bottom();
        } else {
            show_jump_to_latest.set(true);
        }
    });

    let snapshot = state.read().clone();
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/chat_view.css") }
        div { class: "ains-chat__message-pane",
            div {
                id: CHAT_SCROLL_ID,
                class: "ains-chat__scroll no-scrollbar",
                tabindex: "0",
                role: "region",
                aria_label: t.chat_history_label,
                onscroll: move |event| {
                    let scroll = event.data();
                    let near_bottom = is_near_chat_bottom(
                        scroll.scroll_top(),
                        scroll.client_height(),
                        scroll.scroll_height(),
                    );
                    follow_latest.set(near_bottom);
                    if near_bottom {
                        show_jump_to_latest.set(false);
                    }
                },
                if snapshot.items.is_empty() && snapshot.streaming_text.is_empty() {
                    p { class: "ains-chat__empty", {t.chat_empty_hint} }
                }
                for (key , item) in snapshot
                    .items
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| {
                        // 稳定 key；未经 helper 维护时回退索引（旧行为）
                        let key = snapshot.item_keys.get(idx).copied().unwrap_or(idx as u64);
                        (key, item)
                    })
                {
                    ChatItemRow { key: "{key}", item: item.clone(), markdown_key: key }
                }
                if !snapshot.streaming_text.is_empty() {
                    // 流式增量到达频繁。落定前保持纯文本，避免每个 delta 都
                    // 重新解析全文并替换整段 HTML；TurnComplete 后会作为普通
                    // Text 条目以完整 GitHub Markdown 渲染。
                    TextMessage {
                        role: ChatRole::Assistant,
                        text: snapshot.streaming_text,
                        markdown_key: u64::MAX,
                        render_markdown: false,
                    }
                } else if snapshot.busy {
                    div { class: "ains-chat__thinking", {t.chat_thinking} }
                }
            }
            if show_jump_to_latest() {
                button {
                    class: "ains-chat__jump-latest",
                    r#type: "button",
                    aria_label: t.chat_scroll_to_bottom,
                    onclick: move |_| {
                        follow_latest.set(true);
                        show_jump_to_latest.set(false);
                        scroll_chat_to_bottom();
                    },
                    ChevronDown { class: "ains-chat__jump-latest-icon" }
                    span { {t.chat_scroll_to_bottom} }
                }
            }
        }
    }
}

#[component]
fn ChatItemRow(item: ChatItem, markdown_key: u64) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    match item {
        ChatItem::Text { role, text } => rsx! { TextMessage { role, text, markdown_key } },
        ChatItem::ToolCall {
            name,
            input_preview,
            status,
            output_preview,
            ..
        } => rsx! {
            ToolCallCard { name, input_preview, status, output_preview }
        },
        ChatItem::StatusNote { text } => rsx! {
            div { class: "ains-chat__note", "{text}" }
        },
        ChatItem::CompactNote { phase } => rsx! {
            div { class: "ains-chat__note ains-chat__note--compact",
                {tf(t.chat_compacting, &[("phase", &phase)])}
            }
        },
        ChatItem::ErrorNote { text, recoverable } => {
            let label = if recoverable {
                t.chat_error_recoverable
            } else {
                t.chat_error_fatal
            };
            rsx! {
                div { class: "ains-chat__note ains-chat__note--error",
                    span { class: "ains-chat__error-label", {label} }
                    "{text}"
                }
            }
        }
    }
}

/// Whether a Markdown link/image destination is safe to place in a rendered
/// HTML attribute.  Markdown comes from both the model and conversation
/// history, so it must be treated as untrusted input.
fn is_safe_markdown_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('#') {
        return true;
    }

    // A destination without a scheme is a same-origin relative URL.  Reject
    // protocol-relative and backslash forms because browsers may interpret
    // them as cross-origin URLs.
    if !trimmed.starts_with("//")
        && !trimmed.starts_with("/\\")
        && !trimmed.starts_with('\\')
        && !trimmed.contains(':')
    {
        return true;
    }

    trimmed.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("https")
            || scheme.eq_ignore_ascii_case("http")
            || scheme.eq_ignore_ascii_case("mailto")
    })
}

/// The production web CSP permits only same-origin images.  Keep this policy
/// in the shared renderer too, so Web and Desktop don't display different
/// content or silently fail to load remote images.
fn is_safe_same_origin_image_url(url: &str) -> bool {
    let trimmed = url.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with("//")
        && !trimmed.starts_with("/\\")
        && (trimmed.starts_with('/')
            || trimmed.starts_with("./")
            || trimmed.starts_with("../")
            || (!trimmed.starts_with('\\') && !trimmed.contains(':')))
}

#[derive(Clone, Copy)]
enum ImageRenderMode {
    Image,
    Link,
    AltText,
}

fn is_email_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'.' | b'!'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
                | b'-'
                | b'@'
        )
}

/// Find a GFM-style bare HTTP(S) URL or email address in a plain text node.
/// The parser already separates Markdown links and code from text events, so
/// this deliberately runs only on text that is safe to turn into a link.
fn find_bare_autolink(text: &str, from: usize) -> Option<(usize, usize, LinkType, String)> {
    let bytes = text.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        let previous_is_word = index > 0
            && (bytes[index - 1].is_ascii_alphanumeric()
                || matches!(bytes[index - 1], b'.' | b'-' | b'_' | b'/'));
        // Email local parts accept substantially more punctuation than URLs.
        // Treat that whole sequence as one candidate so malformed input such
        // as `a!a!a` is scanned once rather than once per segment.
        let previous_is_email_token = index > 0 && is_email_token_byte(bytes[index - 1]);

        let remaining = &text[index..];
        let url_prefix = if remaining
            .get(.."https://".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        {
            Some(("https://".len(), false))
        } else if remaining
            .get(.."http://".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        {
            Some(("http://".len(), false))
        } else if remaining
            .get(.."www.".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("www."))
        {
            Some(("www.".len(), true))
        } else {
            None
        };
        if !previous_is_word && let Some((scheme_len, add_http_scheme)) = url_prefix {
            let mut end = index + scheme_len;
            let mut opening_parentheses = 0usize;
            let mut closing_parentheses = 0usize;
            while end < bytes.len()
                && !bytes[end].is_ascii_whitespace()
                && !matches!(bytes[end], b'<' | b'>' | b'"' | b'\'')
            {
                match bytes[end] {
                    b'(' => opening_parentheses += 1,
                    b')' => closing_parentheses += 1,
                    _ => {}
                }
                end += 1;
            }
            let mut link_end = end;
            while link_end > index + scheme_len
                && matches!(bytes[link_end - 1], b'.' | b',' | b'!' | b'?' | b';' | b':')
            {
                link_end -= 1;
            }
            while link_end > index + scheme_len
                && bytes[link_end - 1] == b')'
                && closing_parentheses > opening_parentheses
            {
                link_end -= 1;
                closing_parentheses -= 1;
            }
            if link_end > index + scheme_len {
                let url = text[index..link_end].to_string();
                let destination = if add_http_scheme {
                    format!("http://{url}")
                } else {
                    url
                };
                return Some((index, link_end, LinkType::Autolink, destination));
            }
        }

        if !previous_is_email_token && bytes[index].is_ascii_alphanumeric() {
            let mut end = index;
            while end < bytes.len() && is_email_token_byte(bytes[end]) {
                end += 1;
            }
            let mut email_end = end;
            while email_end > index && bytes[email_end - 1] == b'.' {
                email_end -= 1;
            }
            let candidate = &text[index..email_end];
            if let Some((local, domain)) = candidate.split_once('@')
                && !local.is_empty()
                && !domain.is_empty()
                && !domain.starts_with('-')
                && !domain.ends_with('-')
                && domain.contains('.')
                && !domain.contains('@')
            {
                return Some((index, email_end, LinkType::Email, candidate.to_string()));
            }
        }

        index += text[index..]
            .chars()
            .next()
            .expect("index is within a UTF-8 string")
            .len_utf8();
    }
    None
}

fn autolink_text_events(text: &str) -> Vec<Event<'static>> {
    let mut events = Vec::new();
    let mut cursor = 0;
    while let Some((start, end, link_type, destination)) = find_bare_autolink(text, cursor) {
        if cursor < start {
            events.push(Event::Text(text[cursor..start].to_string().into()));
        }
        events.push(Event::Start(Tag::Link {
            link_type,
            dest_url: destination.into(),
            title: "".into(),
            id: "".into(),
        }));
        events.push(Event::Text(text[start..end].to_string().into()));
        events.push(Event::End(TagEnd::Link));
        cursor = end;
    }
    if cursor < text.len() {
        events.push(Event::Text(text[cursor..].to_string().into()));
    }
    events
}

/// A message-local footnote name prevents duplicate DOM IDs when several chat
/// turns use common labels such as `[^1]`.
fn namespaced_footnote_name(message_key: u64, name: &str) -> String {
    format!("ains-footnote-{message_key}-{name}")
}

/// Render the GitHub-flavored Markdown subset used by agent replies.
///
/// `pulldown-cmark` escapes text but intentionally passes raw HTML through.
/// Convert those events back to text and drop unsafe link wrappers before using
/// Dioxus's `dangerous_inner_html`; the generated markup then contains only
/// parser-owned tags and escaped untrusted content.
fn render_github_markdown(markdown: &str, message_key: u64) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut image_modes = Vec::new();
    // Keep an entry for each source link so that its closing event mirrors the
    // decision made at its opening event.  An unsafe destination must not turn
    // into `href=""`: browsers interpret that as a navigation to this page.
    let mut link_modes = Vec::new();
    let mut link_depth = 0usize;
    let parser = Parser::new_ext(markdown, options).filter_map(move |event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Some(Event::Text(html)),
        Event::FootnoteReference(name) => Some(Event::FootnoteReference(
            namespaced_footnote_name(message_key, &name).into(),
        )),
        Event::Start(Tag::FootnoteDefinition(name)) => Some(Event::Start(Tag::FootnoteDefinition(
            namespaced_footnote_name(message_key, &name).into(),
        ))),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let is_safe = link_type == LinkType::Email || is_safe_markdown_url(&dest_url);
            link_modes.push(is_safe);
            link_depth += 1;
            is_safe.then_some(Event::Start(Tag::Link {
                link_type,
                // Keep an email autolink exactly as parsed.  Other URLs are
                // normalized before insertion so whitespace-only destinations
                // cannot become an empty-document navigation.
                dest_url: if link_type == LinkType::Email {
                    dest_url
                } else {
                    dest_url.trim().to_string().into()
                },
                title,
                id,
            }))
        }
        Event::End(TagEnd::Link) => {
            link_depth = link_depth.saturating_sub(1);
            link_modes
                .pop()
                .unwrap_or(false)
                .then_some(Event::End(TagEnd::Link))
        }
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let mode = if is_safe_same_origin_image_url(&dest_url) {
                ImageRenderMode::Image
            } else if is_safe_markdown_url(&dest_url) && link_depth == 0 {
                // Remote images are blocked by the Web CSP.  Retain their
                // useful destination without making a background request.
                // A link cannot contain another link, so inside an existing
                // anchor we instead preserve just the image's alt text.
                ImageRenderMode::Link
            } else {
                ImageRenderMode::AltText
            };
            image_modes.push(mode);
            match mode {
                ImageRenderMode::Image => Some(Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                })),
                ImageRenderMode::Link => {
                    link_depth += 1;
                    Some(Event::Start(Tag::Link {
                        link_type,
                        dest_url,
                        title,
                        id,
                    }))
                }
                ImageRenderMode::AltText => None,
            }
        }
        Event::End(TagEnd::Image) => match image_modes.pop() {
            Some(ImageRenderMode::Image) => Some(Event::End(TagEnd::Image)),
            Some(ImageRenderMode::Link) => {
                link_depth = link_depth.saturating_sub(1);
                Some(Event::End(TagEnd::Link))
            }
            Some(ImageRenderMode::AltText) | None => None,
        },
        event => Some(event),
    });
    let mut link_depth = 0usize;
    let mut image_depth = 0usize;
    let mut code_block_depth = 0usize;
    let parser = parser.flat_map(move |event| match event {
        Event::Start(tag @ Tag::Link { .. }) => {
            link_depth += 1;
            vec![Event::Start(tag)]
        }
        Event::End(TagEnd::Link) => {
            link_depth = link_depth.saturating_sub(1);
            vec![Event::End(TagEnd::Link)]
        }
        Event::Start(tag @ Tag::Image { .. }) => {
            image_depth += 1;
            vec![Event::Start(tag)]
        }
        Event::End(TagEnd::Image) => {
            image_depth = image_depth.saturating_sub(1);
            vec![Event::End(TagEnd::Image)]
        }
        Event::Start(tag @ Tag::CodeBlock(_)) => {
            code_block_depth += 1;
            vec![Event::Start(tag)]
        }
        Event::End(TagEnd::CodeBlock) => {
            code_block_depth = code_block_depth.saturating_sub(1);
            vec![Event::End(TagEnd::CodeBlock)]
        }
        Event::Text(text) if link_depth == 0 && image_depth == 0 && code_block_depth == 0 => {
            autolink_text_events(&text)
        }
        event => vec![event],
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

/// 单条文本消息。操作区位于消息框外侧，以便与消息内容清晰分隔。
#[component]
fn TextMessage(
    role: ChatRole,
    text: String,
    /// Stable ChatView item key used to namespace generated Markdown IDs.
    markdown_key: u64,
    /// Streaming content is rendered as text until the final message arrives;
    /// parsing every partial response would repeatedly rebuild a growing HTML
    /// subtree and make long replies lag.
    #[props(default = true)]
    render_markdown: bool,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    let (message_class, bubble_class, meta_class, label) = match role {
        ChatRole::User => (
            "ains-chat__message ains-chat__message--user",
            "ains-chat__msg ains-chat__msg--user",
            "ains-chat__message-meta ains-chat__message-meta--user",
            t.chat_role_user,
        ),
        ChatRole::Assistant => (
            "ains-chat__message ains-chat__message--assistant",
            "ains-chat__msg ains-chat__msg--assistant",
            "ains-chat__message-meta ains-chat__message-meta--assistant",
            t.chat_role_assistant,
        ),
        ChatRole::System => (
            "ains-chat__message ains-chat__message--system",
            "ains-chat__msg ains-chat__msg--system",
            "ains-chat__message-meta ains-chat__message-meta--system",
            "",
        ),
    };

    let markdown_html = render_markdown.then(|| render_github_markdown(&text, markdown_key));

    rsx! {
        div { class: message_class,
            div { class: bubble_class,
                if !label.is_empty() {
                    span { class: "ains-chat__role", {label} }
                }
                if let Some(markdown_html) = markdown_html {
                    div {
                        class: "ains-chat__text ains-chat__markdown",
                        dangerous_inner_html: "{markdown_html}",
                    }
                } else {
                    pre { class: "ains-chat__text ains-chat__text--plain", "{text}" }
                }
            }
            div { class: meta_class,
                MessageCopyButton { text }
            }
        }
    }
}

/// 单条文本消息的复制操作。状态保存在行组件内，避免其他消息的反馈被影响。
#[component]
fn MessageCopyButton(text: String) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    let mut copy_state = use_signal(CopyState::default);
    let (class, label) = match copy_state() {
        CopyState::Idle => ("ains-chat__copy", t.chat_copy),
        CopyState::Copied => ("ains-chat__copy ains-chat__copy--copied", t.chat_copied),
        CopyState::Failed => (
            "ains-chat__copy ains-chat__copy--failed",
            t.chat_copy_failed,
        ),
    };

    rsx! {
        button {
            class,
            r#type: "button",
            aria_label: label,
            title: label,
            onclick: move |_| {
                let text = text.clone();
                spawn(async move {
                    copy_state.set(if copy_message_text(text).await {
                        CopyState::Copied
                    } else {
                        CopyState::Failed
                    });
                });
            },
            if copy_state() == CopyState::Copied {
                Check { class: "ains-chat__copy-icon" }
            } else {
                Copy { class: "ains-chat__copy-icon" }
            }
            span { class: "ains-chat__sr-only", aria_live: "polite", "{label}" }
        }
    }
}

/// 工具调用卡片（折叠展开输入/输出预览）。
#[component]
pub fn ToolCallCard(
    name: String,
    input_preview: String,
    status: ToolCallStatus,
    output_preview: String,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    let mut expanded = use_signal(|| false);

    let (badge_class, badge_label) = match status {
        ToolCallStatus::Running => ("ains-chat__tool-badge ains-chat__tool-badge--running", {
            t.chat_tool_running
        }),
        ToolCallStatus::Done => ("ains-chat__tool-badge ains-chat__tool-badge--done", {
            t.chat_tool_done
        }),
        ToolCallStatus::Failed => ("ains-chat__tool-badge ains-chat__tool-badge--failed", {
            t.chat_tool_failed
        }),
    };

    rsx! {
        div { class: "ains-chat__tool",
            button {
                class: "ains-chat__tool-header",
                r#type: "button",
                onclick: move |_| expanded.toggle(),
                if expanded() {
                    ChevronDown { class: "ains-chat__tool-chevron" }
                } else {
                    ChevronRight { class: "ains-chat__tool-chevron" }
                }
                span { class: "ains-chat__tool-name", "{name}" }
                span { class: badge_class, {badge_label} }
            }
            if expanded() {
                div { class: "ains-chat__tool-body",
                    div { class: "ains-chat__tool-section-label", {t.chat_tool_input_label} }
                    pre { class: "ains-chat__tool-pre", "{input_preview}" }
                    if status != ToolCallStatus::Running {
                        div { class: "ains-chat__tool-section-label", {t.chat_tool_output_label} }
                        pre { class: "ains-chat__tool-pre", "{output_preview}" }
                    }
                }
            }
        }
    }
}

/// Slash 命令视图模型（Phase 6.12）：`name` 形如 `/skill`。
#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommandView {
    pub name: String,
    pub description: String,
}

/// 输入区 —— 多行输入 + 发送/停止 + Slash 命令建议（Phase 6.3 / 6.12）。
#[component]
pub fn ChatInput(
    busy: bool,
    /// 发送暂不可用，但不显示 Stop（例如等待 Kernel 确认中断）。
    #[props(default)]
    disabled: bool,
    /// 返回 `true` 才清空草稿；宿主拒绝发送时保留输入供稍后重试。
    on_send: Callback<String, bool>,
    on_interrupt: EventHandler<()>,
    /// 可用 Slash 命令；输入以 `/` 起始时展示过滤后的建议下拉。
    #[props(default)]
    slash_commands: Vec<SlashCommandView>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    let mut draft = use_signal(String::new);

    let mut submit = move || {
        let text = draft.read().trim().to_string();
        if text.is_empty() {
            return;
        }
        if on_send.call(text) {
            draft.set(String::new());
        }
    };

    // Slash 建议：输入以 `/` 起始时，按首个 token 前缀过滤命令。
    let draft_val = draft.read().clone();
    let suggestions = filter_slash_suggestions(&draft_val, &slash_commands);

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/chat_view.css") }
        div { class: "ains-chat__composer",
            if !suggestions.is_empty() {
                div { class: "ains-chat__slash",
                    div { class: "ains-chat__slash-hint", {t.slash_hint} }
                    for cmd in suggestions {
                        button {
                            key: "{cmd.name}",
                            class: "ains-chat__slash-item",
                            r#type: "button",
                            onclick: {
                                let name = cmd.name.clone();
                                move |_| draft.set(format!("{name} "))
                            },
                            span { class: "ains-chat__slash-name", "{cmd.name}" }
                            span { class: "ains-chat__slash-desc", "{cmd.description}" }
                        }
                    }
                }
            }
            div { class: "ains-chat__input-row",
                textarea {
                    class: "ains-chat__textarea",
                    placeholder: t.chat_input_placeholder,
                    value: "{draft}",
                    rows: 2,
                    disabled: busy || disabled,
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter && !e.modifiers().shift() {
                            e.prevent_default();
                            if !busy && !disabled {
                                submit();
                            }
                        }
                    },
                }
                if busy {
                    button {
                        class: "ains-chat__btn ains-chat__btn--stop",
                        r#type: "button",
                        aria_label: t.chat_stop,
                        onclick: move |_| on_interrupt.call(()),
                        CircleStop {}
                        span { {t.chat_stop} }
                    }
                } else {
                    button {
                        class: "ains-chat__btn ains-chat__btn--send",
                        r#type: "button",
                        aria_label: t.chat_send,
                        disabled,
                        onclick: move |_| submit(),
                        SendHorizontal {}
                        span { {t.chat_send} }
                    }
                }
            }
        }
    }
}

/// Slash 命令建议过滤（纯函数，供单测固化）：草稿以 `/` 起始时，
/// 尚未输入空白时按命令前缀匹配；开始填写参数后只保留精确命令。
/// 仅 `/` 时展示全部。
/// 注意：这里是宽松的输入辅助（`/skills` 不匹配 `/skill` 前缀则无
/// 建议）；实际执行的严格 token 匹配在宿主 `on_send` 侧把关。
fn filter_slash_suggestions(draft: &str, commands: &[SlashCommandView]) -> Vec<SlashCommandView> {
    if !draft.starts_with('/') {
        return Vec::new();
    }
    let token = draft.split_whitespace().next().unwrap_or("");
    // `/skill` and `/skill-create` deliberately share a prefix.  Once the
    // user has typed the command separator, offering the latter would replace
    // an in-progress `/skill <name>` command when the suggestion is clicked.
    let has_command_separator = draft[token.len()..].chars().any(char::is_whitespace);
    commands
        .iter()
        .filter(|c| {
            if has_command_separator {
                c.name == token
            } else {
                token == "/" || c.name.starts_with(token)
            }
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_bottom_tolerance_only_follows_when_visually_near_end() {
        assert!(is_near_chat_bottom(552.0, 400, 1_000));
        assert!(is_near_chat_bottom(560.5, 400, 1_000));
        assert!(!is_near_chat_bottom(551.5, 400, 1_000));
        assert!(!is_near_chat_bottom(100.0, 400, 1_000));
    }

    fn commands() -> Vec<SlashCommandView> {
        vec![
            SlashCommandView {
                name: "/skill".into(),
                description: "run a skill".into(),
            },
            SlashCommandView {
                name: "/skill-create".into(),
                description: "create a skill".into(),
            },
            SlashCommandView {
                name: "/help".into(),
                description: "show help".into(),
            },
        ]
    }

    #[test]
    fn slash_suggestions_filter_by_first_token_prefix() {
        let cmds = commands();
        // 仅 `/` → 全部命令
        assert_eq!(filter_slash_suggestions("/", &cmds).len(), 3);
        // 前缀匹配：`/sk` → /skill、/skill-create；`/he` → /help
        let hits = filter_slash_suggestions("/sk", &cmds);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "/skill");
        assert_eq!(hits[1].name, "/skill-create");
        assert_eq!(filter_slash_suggestions("/he", &cmds)[0].name, "/help");
        // 输入参数（或仅输入分隔空格）后不能展示同前缀的另一条命令，
        // 否则点选会覆写正在输入的 `/skill <name>`。
        for draft in ["/skill ", "/skill csv"] {
            let hits = filter_slash_suggestions(draft, &cmds);
            assert_eq!(hits.len(), 1, "{draft}");
            assert_eq!(hits[0].name, "/skill", "{draft}");
        }
    }

    #[test]
    fn slash_suggestions_empty_for_non_slash_or_unknown() {
        let cmds = commands();
        assert!(filter_slash_suggestions("", &cmds).is_empty());
        assert!(filter_slash_suggestions("hello /skill", &cmds).is_empty());
        // `/skills`：非任何命令的前缀 → 无建议（严格执行在宿主侧）
        assert!(filter_slash_suggestions("/skills x", &cmds).is_empty());
        assert!(filter_slash_suggestions("/unknown", &cmds).is_empty());
    }

    #[test]
    fn stable_keys_survive_mid_list_removal() {
        // 回归（Code Review 非阻断项 #2）：中段删除后剩余条目的
        // key 不变且不复用，行组件本地状态（工具卡片展开位）不错位
        let mut state = ChatViewState::default();
        for text in ["a", "b", "c"] {
            state.push_item(ChatItem::Text {
                role: ChatRole::User,
                text: text.into(),
            });
        }
        assert_eq!(state.item_keys, [0, 1, 2]);

        state.remove_item_at(1);
        assert_eq!(state.item_keys, [0, 2], "剩余 key 不变、不复用");
        assert_eq!(state.items.len(), 2);

        state.push_item(ChatItem::StatusNote { text: "s".into() });
        assert_eq!(state.item_keys, [0, 2, 3], "新 key 单调递增");

        // set_items 整体替换：全部重新分配，不与旧 key 重叠
        state.set_items(vec![ChatItem::Text {
            role: ChatRole::Assistant,
            text: "x".into(),
        }]);
        assert_eq!(state.item_keys, [4]);
    }

    #[test]
    fn clipboard_script_escapes_message_text_and_falls_back_after_api_rejection() {
        let message = "quote: \"; newline:\n; unicode: 智能体".to_string();
        let script = clipboard_copy_script(&message);
        assert!(script.contains(r#"quote: \""#));
        assert!(script.contains(r#"newline:\n"#));

        // Keep the fallback after the Clipboard API call: browsers may expose
        // `navigator.clipboard` but reject writes because of permissions.
        assert!(script.contains("catch (_)"));
        assert!(script.find("catch (_)").unwrap() < script.find("document.execCommand").unwrap());
    }

    #[test]
    fn github_markdown_renders_common_agent_response_elements() {
        let html = render_github_markdown(
            "# Title\n\n**bold** and `code`\n\n- [x] done\n- [ ] todo\n\n| A | B |\n| - | - |\n| 1 | 2 |\n\n~~old~~",
            1,
        );
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("type=\"checkbox\" checked=\"\""));
        assert!(html.contains("<table>"));
        assert!(html.contains("<del>old</del>"));
    }

    #[test]
    fn github_markdown_escapes_raw_html_and_unsafe_urls() {
        let html = render_github_markdown(
            "<script>alert('xss')</script>\n\n[bad](javascript:alert(1))\n\n[good](https://example.com)",
            1,
        );
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("javascript:"));
        assert!(html.contains("<p>bad</p>"));
        assert!(!html.contains("href=\"\""));
        assert!(html.contains("href=\"https://example.com\""));

        let empty = render_github_markdown("[empty]()", 1);
        assert_eq!(empty, "<p>empty</p>\n");
    }

    #[test]
    fn markdown_url_allowlist_accepts_case_insensitive_protocols() {
        for url in [
            "HTTPS://example.com/docs",
            "Http://example.com/docs",
            "MAILTO:help@example.com",
        ] {
            assert!(is_safe_markdown_url(url), "{url}");
        }
        assert!(!is_safe_markdown_url("javascript:alert(1)"));
        assert!(!is_safe_markdown_url("data:text/html,unsafe"));
        assert!(!is_safe_markdown_url("  "));

        let html = render_github_markdown("[docs](HTTPS://example.com/docs)", 1);
        assert!(html.contains("href=\"HTTPS://example.com/docs\""));
    }

    #[test]
    fn github_markdown_keeps_relative_and_email_links() {
        let html = render_github_markdown(
            "[relative](docs/getting-started.md)\n\n<help@example.com>",
            1,
        );
        assert!(html.contains("href=\"docs/getting-started.md\""));
        assert!(html.contains("href=\"mailto:help@example.com\""));
    }

    #[test]
    fn markdown_images_follow_the_web_csp_policy() {
        let html = render_github_markdown(
            "![local](/assets/logo.png)\n\n![remote](https://example.com/logo.png)\n\n![unsafe](javascript:alert(1))",
            1,
        );
        assert!(html.contains("<img src=\"/assets/logo.png\" alt=\"local\""));
        assert!(html.contains("<a href=\"https://example.com/logo.png\">remote</a>"));
        assert!(!html.contains("<img src=\"https://example.com/logo.png\""));
        assert!(!html.contains("javascript:"));
        assert!(html.contains("unsafe"));

        assert!(is_safe_same_origin_image_url("/assets/logo.png"));
        assert!(is_safe_same_origin_image_url("../assets/logo.png"));
        assert!(is_safe_same_origin_image_url("assets/logo.png"));
        assert!(!is_safe_same_origin_image_url("//example.com/logo.png"));
        assert!(!is_safe_same_origin_image_url(
            "https://example.com/logo.png"
        ));
    }

    #[test]
    fn remote_image_inside_a_link_keeps_the_outer_link() {
        let html =
            render_github_markdown("[![preview](https://example.com/preview.png)](/details)", 1);
        assert_eq!(html, "<p><a href=\"/details\">preview</a></p>\n");
        assert!(!html.contains("https://example.com/preview.png"));
    }

    #[test]
    fn unsafe_outer_link_does_not_restore_a_remote_image_link() {
        let html = render_github_markdown(
            "[![preview](https://example.com/preview.png)](javascript:alert(1))",
            1,
        );
        assert_eq!(html, "<p>preview</p>\n");
        assert!(!html.contains("href="));
    }

    #[test]
    fn user_markdown_readability_rules_preserve_contrast() {
        let css = include_str!("../assets/styling/chat_view.css");
        assert!(css.contains(".ains-chat__msg--user .ains-chat__markdown blockquote"));
        assert!(css.contains("blockquote {\n  color: inherit;\n}"));
        assert!(css.contains(".ains-chat__msg--user .ains-chat__markdown code"));
        assert!(css.contains(".ains-chat__msg--user .ains-chat__markdown th"));
        assert!(css.contains("color: var(--color-text-primary)"));
    }

    #[test]
    fn footnotes_are_namespaced_by_stable_message_key() {
        let first = render_github_markdown("first[^1]\n\n[^1]: note", 41);
        let second = render_github_markdown("second[^1]\n\n[^1]: note", 42);

        assert!(first.contains("href=\"#ains-footnote-41-1\""));
        assert!(first.contains("id=\"ains-footnote-41-1\""));
        assert!(second.contains("href=\"#ains-footnote-42-1\""));
        assert!(second.contains("id=\"ains-footnote-42-1\""));
        assert_ne!(first, second);
    }

    #[test]
    fn github_markdown_autolinks_bare_urls_and_emails_without_touching_code_or_links() {
        let html = render_github_markdown(
            "说明： https://example.com/guide. Mirror: www.example.com. Upper: HTTPS://EXAMPLE.COM/path. WWW: WWW.Example.com. Email: help@example.com.\n\n`https://example.com/code`\n\n[existing](https://example.com/existing)",
            1,
        );
        assert!(
            html.contains("<a href=\"https://example.com/guide\">https://example.com/guide</a>.")
        );
        assert!(html.contains("<a href=\"mailto:help@example.com\">help@example.com</a>."));
        assert!(html.contains("<a href=\"http://www.example.com\">www.example.com</a>."));
        assert!(
            html.contains("<a href=\"HTTPS://EXAMPLE.COM/path\">HTTPS://EXAMPLE.COM/path</a>.")
        );
        assert!(html.contains("<a href=\"http://WWW.Example.com\">WWW.Example.com</a>."));
        assert!(html.contains("<code>https://example.com/code</code>"));
        assert_eq!(
            html.matches("href=\"https://example.com/existing\"")
                .count(),
            1
        );
    }

    #[test]
    fn bare_autolink_trims_many_unmatched_closing_parentheses_in_linear_time() {
        let trailing_parentheses = ")".repeat(4_096);
        let input = format!("https://example.com/path{trailing_parentheses}");

        let (start, end, link_type, destination) = find_bare_autolink(&input, 0).unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, "https://example.com/path".len());
        assert_eq!(link_type, LinkType::Autolink);
        assert_eq!(destination, "https://example.com/path");
    }

    #[test]
    fn email_autolink_scans_punctuation_token_once_and_keeps_valid_addresses() {
        let malformed = "a!".repeat(4_096);
        assert!(find_bare_autolink(&malformed, 0).is_none());

        let html = render_github_markdown("Contact ops+alerts@example.com.", 1);
        assert!(
            html.contains("<a href=\"mailto:ops+alerts@example.com\">ops+alerts@example.com</a>.")
        );
    }
}
