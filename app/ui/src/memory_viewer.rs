use dioxus::prelude::*;
use dioxus_icons::lucide::Trash2;

use crate::{Badge, BadgeVariant, EN, I18nContext, Modal, tf};

/// 记忆条目视图模型（宿主从 `MemdirEntry` 映射）。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCard {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    /// 0.0–1.0 重要度（宿主原样传入）。
    pub importance: f64,
    pub tags: Vec<String>,
    /// 格式化后的创建时间。
    pub created_at: String,
    /// 记忆正文（详情抽屉展示）。
    pub body: String,
}

/// Memory 浏览器（Phase 6.6）：列出 memdir 长期记忆，可查看、删除和清空。
#[component]
pub fn MemoryViewer(
    memories: ReadSignal<Vec<MemoryCard>>,
    has_durable_memories: bool,
    on_delete: EventHandler<String>,
    on_clear_all: EventHandler<()>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    let mut detail = use_signal(|| None::<MemoryCard>);
    let mut pending_delete = use_signal(|| None::<MemoryCard>);
    let mut clear_confirm_open = use_signal(|| false);

    let cards = memories.read().clone();
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/memory_viewer.css") }
        section { class: "ains-memory",
            header { class: "ains-memory__header",
                div {
                    h2 { class: "ains-memory__title", {t.memory_title} }
                    p { class: "ains-memory__subtitle", {t.memory_subtitle} }
                }
                if !cards.is_empty() || has_durable_memories {
                    button {
                        class: "ains-memory__clear",
                        r#type: "button",
                        onclick: move |_| clear_confirm_open.set(true),
                        {t.memory_clear_all_btn}
                    }
                }
            }
            if cards.is_empty() {
                p { class: "ains-memory__empty", {t.memory_empty} }
            }
            div { class: "ains-memory__grid",
                for card in cards {
                    button {
                        key: "{card.id}",
                        class: "ains-memory__card",
                        r#type: "button",
                        onclick: {
                            let c = card.clone();
                            move |_| detail.set(Some(c.clone()))
                        },
                        div { class: "ains-memory__card-head",
                            span { class: "ains-memory__card-name", "{card.name}" }
                            // 百分比徽标无文字标签，title 补充语义（悬停/读屏）
                            span { title: t.memory_column_importance,
                                Badge { variant: importance_variant(card.importance),
                                    {format!("{:.0}%", (card.importance.clamp(0.0, 1.0)) * 100.0)}
                                }
                            }
                        }
                        p { class: "ains-memory__card-desc", "{card.description}" }
                        div { class: "ains-memory__card-meta",
                            span { "{t.memory_column_category}: {card.category}" }
                            span { "{card.created_at}" }
                        }
                    }
                }
            }
        }

        if let Some(m) = detail.read().clone() {
            Modal {
                title: format!("{} — {}", t.memory_detail_title, m.name),
                on_close: move |_| detail.set(None),
                div { class: "ains-memory__detail",
                    p { class: "ains-memory__detail-desc", "{m.description}" }
                    if !m.tags.is_empty() {
                        div { class: "ains-memory__detail-tags",
                            span { class: "ains-memory__detail-label", "{t.memory_tags}:" }
                            for tag in m.tags.clone() {
                                span { key: "{tag}", class: "ains-memory__tag", "{tag}" }
                            }
                        }
                    }
                    pre { class: "ains-memory__body", "{m.body}" }
                    button {
                        class: "ains-memory__delete",
                        r#type: "button",
                        onclick: {
                            let memory = m.clone();
                            move |_| pending_delete.set(Some(memory.clone()))
                        },
                        Trash2 {}
                        {t.memory_delete_btn}
                    }
                }
            }
        }

        if let Some(memory) = pending_delete.read().clone() {
            Modal {
                title: t.memory_confirm_delete_title.to_string(),
                on_close: move |_| pending_delete.set(None),
                div { class: "ains-memory__confirm",
                    p { {tf(t.memory_confirm_delete_msg, &[("name", &memory.name)])} }
                    div { class: "ains-memory__confirm-actions",
                        button {
                            r#type: "button",
                            onclick: move |_| pending_delete.set(None),
                            {t.modal_close}
                        }
                        button {
                            class: "ains-memory__delete",
                            r#type: "button",
                            onclick: {
                                let id = memory.id.clone();
                                move |_| {
                                    pending_delete.set(None);
                                    detail.set(None);
                                    on_delete.call(id.clone());
                                }
                            },
                            {t.memory_delete_btn}
                        }
                    }
                }
            }
        }

        if clear_confirm_open() {
            Modal {
                title: t.memory_confirm_clear_all_title.to_string(),
                on_close: move |_| clear_confirm_open.set(false),
                div { class: "ains-memory__confirm",
                    p { {t.memory_confirm_clear_all_msg} }
                    div { class: "ains-memory__confirm-actions",
                        button {
                            r#type: "button",
                            onclick: move |_| clear_confirm_open.set(false),
                            {t.modal_close}
                        }
                        button {
                            class: "ains-memory__delete",
                            r#type: "button",
                            onclick: move |_| {
                                clear_confirm_open.set(false);
                                detail.set(None);
                                on_clear_all.call(());
                            },
                            {t.memory_clear_all_btn}
                        }
                    }
                }
            }
        }
    }
}

fn importance_variant(importance: f64) -> BadgeVariant {
    if importance >= 0.75 {
        BadgeVariant::Success
    } else if importance >= 0.4 {
        BadgeVariant::User
    } else {
        BadgeVariant::Warning
    }
}
