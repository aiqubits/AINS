use dioxus::prelude::*;

use crate::{Badge, BadgeVariant, EN, I18nContext, Modal};

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

/// Memory 浏览器（Phase 6.6）：列出 memdir 长期记忆，可查看详情。
#[component]
pub fn MemoryViewer(memories: ReadSignal<Vec<MemoryCard>>) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    let mut detail = use_signal(|| None::<MemoryCard>);

    let cards = memories.read().clone();
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/memory_viewer.css") }
        section { class: "ains-memory",
            header { class: "ains-memory__header",
                h2 { class: "ains-memory__title", {t.memory_title} }
                p { class: "ains-memory__subtitle", {t.memory_subtitle} }
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
