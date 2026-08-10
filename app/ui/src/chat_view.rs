use dioxus::prelude::*;
use dioxus_icons::lucide::{ChevronDown, ChevronRight, CircleStop, SendHorizontal};

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

/// Chat 对话视图 —— 消息列表 + 流式尾部 + 自动滚动（Phase 6.3）。
#[component]
pub fn ChatView(state: ReadSignal<ChatViewState>) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    // 条目或流式文本变化时滚动到底部（与 CodeConsole 同一模式）。
    use_effect(move || {
        let snapshot = state.read();
        if snapshot.items.is_empty() && snapshot.streaming_text.is_empty() {
            return;
        }
        spawn(async move {
            let _ = document::eval(
                "const el = document.getElementById('ains-chat-scroll'); \
                 if (el) { el.scrollTop = el.scrollHeight; }",
            )
            .await;
        });
    });

    let snapshot = state.read().clone();
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/chat_view.css") }
        div { id: "ains-chat-scroll", class: "ains-chat__scroll no-scrollbar",
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
                div { class: "ains-chat__msg ains-chat__msg--assistant",
                    span { class: "ains-chat__role", {t.chat_role_assistant} }
                    pre { class: "ains-chat__text", "{snapshot.streaming_text}" }
                }
            } else if snapshot.busy {
                div { class: "ains-chat__thinking", {t.chat_thinking} }
            }
        }
    }
}

#[component]
fn ChatItemRow(item: ChatItem) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    match item {
        ChatItem::Text { role, text } => {
            let (class, label) = match role {
                ChatRole::User => ("ains-chat__msg ains-chat__msg--user", t.chat_role_user),
                ChatRole::Assistant => (
                    "ains-chat__msg ains-chat__msg--assistant",
                    t.chat_role_assistant,
                ),
                ChatRole::System => ("ains-chat__msg ains-chat__msg--system", ""),
            };
            rsx! {
                div { class,
                    if !label.is_empty() {
                        span { class: "ains-chat__role", {label} }
                    }
                    pre { class: "ains-chat__text", "{text}" }
                }
            }
        }
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
/// 按首个空白分隔 token 前缀匹配命令名；仅 `/` 时展示全部。
/// 注意：这里是宽松的输入辅助（`/skills` 不匹配 `/skill` 前缀则无
/// 建议）；实际执行的严格 token 匹配在宿主 `on_send` 侧把关。
fn filter_slash_suggestions(draft: &str, commands: &[SlashCommandView]) -> Vec<SlashCommandView> {
    if !draft.starts_with('/') {
        return Vec::new();
    }
    let token = draft.split_whitespace().next().unwrap_or("");
    commands
        .iter()
        .filter(|c| token == "/" || c.name.starts_with(token))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<SlashCommandView> {
        vec![
            SlashCommandView {
                name: "/skill".into(),
                description: "run a skill".into(),
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
        assert_eq!(filter_slash_suggestions("/", &cmds).len(), 2);
        // 前缀匹配：`/sk` → /skill；`/he` → /help
        let hits = filter_slash_suggestions("/sk", &cmds);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "/skill");
        assert_eq!(filter_slash_suggestions("/he", &cmds)[0].name, "/help");
        // 完整命令后跟参数：仅按首 token 匹配，不受参数影响
        assert_eq!(filter_slash_suggestions("/skill csv", &cmds).len(), 1);
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
}
