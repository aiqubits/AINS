use dioxus::prelude::*;
use dioxus_icons::lucide::{Check, ChevronDown, ChevronRight, CircleStop, Copy, SendHorizontal};

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
    /// 普通文本消息（Markdown 原文按纯文本渲染，保留换行）。
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
                    ChatItemRow { key: "{key}", item: item.clone() }
                }
                if !snapshot.streaming_text.is_empty() {
                    TextMessage { role: ChatRole::Assistant, text: snapshot.streaming_text }
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
fn ChatItemRow(item: ChatItem) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    match item {
        ChatItem::Text { role, text } => rsx! { TextMessage { role, text } },
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

/// 单条文本消息。操作区位于消息框外侧，以便与消息内容清晰分隔。
#[component]
fn TextMessage(role: ChatRole, text: String) -> Element {
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

    rsx! {
        div { class: message_class,
            div { class: bubble_class,
                if !label.is_empty() {
                    span { class: "ains-chat__role", {label} }
                }
                pre { class: "ains-chat__text", "{text}" }
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
}
