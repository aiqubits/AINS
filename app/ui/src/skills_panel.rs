use dioxus::prelude::*;
use dioxus_icons::lucide::Trash2;

use crate::{Badge, BadgeVariant, EN, I18nContext, Modal, tf};

/// 技能信任级别（视图模型，宿主从 rust-agent `SkillTrust` 映射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillTrustView {
    System,
    Trusted,
    Generated,
    Temporary,
}

/// 技能卡片（列表条目）。`corrupted=true` 表示存储条目校验失败，
/// 仅可删除不可查看详情。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillCard {
    pub name: String,
    pub description: String,
    pub category: String,
    pub trust: SkillTrustView,
    /// 格式化后的创建时间（宿主负责本地化格式）。
    pub created_at: String,
    pub requires_tools: Vec<String>,
    pub corrupted: bool,
}

/// 技能详情（详情抽屉展示；`frontmatter` 为 YAML 原文）。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDetailView {
    pub name: String,
    pub frontmatter: String,
    pub body: String,
    /// 可回滚版本（保留范围内，如 ["v1.0","v1.2"]；空则隐藏回滚区）。
    pub rollback_versions: Vec<String>,
    /// 当前活跃版本号（如 "v2.0"；空串表示未版本化的旧数据）。
    pub active_version: String,
}

/// Skills 管理面板 —— 浏览 / 删除，无导入入口（Phase 6.4）。
///
/// 数据获取与删除执行由宿主完成：`on_open_detail` 请求加载详情，
/// `on_delete` 在用户二次确认后触发。
#[component]
pub fn SkillsPanel(
    skills: ReadSignal<Vec<SkillCard>>,
    /// 宿主加载完成的详情（None 表示抽屉关闭或加载中）。
    detail: ReadSignal<Option<SkillDetailView>>,
    on_open_detail: EventHandler<String>,
    on_close_detail: EventHandler<()>,
    on_delete: EventHandler<String>,
    on_clear_all: EventHandler<()>,
    /// 回滚到指定版本（(name, version)；Phase 6.9）。
    on_rollback: EventHandler<(String, String)>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    // 待删除技能名（Some 时显示确认弹窗）。
    let mut pending_delete = use_signal(|| None::<String>);
    let mut clear_confirm_open = use_signal(|| false);

    let cards = skills.read().clone();
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/skills_panel.css") }
        section { class: "ains-skills",
            header { class: "ains-skills__header",
                div {
                    h2 { class: "ains-skills__title", {t.skills_title} }
                    p { class: "ains-skills__subtitle", {t.skills_subtitle} }
                }
                if !cards.is_empty() {
                    button {
                        class: "ains-skills__btn ains-skills__btn--danger",
                        r#type: "button",
                        onclick: move |_| clear_confirm_open.set(true),
                        {t.skills_clear_all_btn}
                    }
                }
            }
            if cards.is_empty() {
                p { class: "ains-skills__empty", {t.skills_empty} }
            }
            div { class: "ains-skills__grid",
                for card in cards {
                    SkillCardRow {
                        key: "{card.name}",
                        card: card.clone(),
                        on_open: move |name| on_open_detail.call(name),
                        on_delete_request: move |name| pending_delete.set(Some(name)),
                    }
                }
            }
        }

        // 详情抽屉（Modal 复用）
        if let Some(d) = detail.read().clone() {
            Modal {
                title: format!("{} — {}", t.skills_detail_title, d.name),
                on_close: move |_| on_close_detail.call(()),
                div { class: "ains-skills__detail",
                    if !d.active_version.is_empty() {
                        div { class: "ains-skills__versions",
                            span { class: "ains-skills__versions-label", "Active: {d.active_version}" }
                            for ver in d.rollback_versions.iter().filter(|v| **v != d.active_version) {
                                button {
                                    key: "{ver}",
                                    class: "ains-skills__btn ains-skills__btn--cancel",
                                    r#type: "button",
                                    onclick: {
                                        let name = d.name.clone();
                                        let ver = ver.clone();
                                        move |_| on_rollback.call((name.clone(), ver.clone()))
                                    },
                                    "↩ {ver}"
                                }
                            }
                        }
                    }
                    if !d.frontmatter.is_empty() {
                        pre { class: "ains-skills__frontmatter", "{d.frontmatter}" }
                    }
                    pre { class: "ains-skills__body", "{d.body}" }
                }
            }
        }

        // 删除二次确认
        if let Some(name) = pending_delete.read().clone() {
            Modal {
                title: t.skills_confirm_delete_title.to_string(),
                on_close: move |_| pending_delete.set(None),
                div { class: "ains-skills__confirm",
                    p { {tf(t.skills_confirm_delete_msg, &[("name", &name)])} }
                    div { class: "ains-skills__confirm-actions",
                        button {
                            class: "ains-skills__btn ains-skills__btn--cancel",
                            r#type: "button",
                            onclick: move |_| pending_delete.set(None),
                            {t.modal_close}
                        }
                        button {
                            class: "ains-skills__btn ains-skills__btn--danger",
                            r#type: "button",
                            onclick: {
                                let name = name.clone();
                                move |_| {
                                    pending_delete.set(None);
                                    on_delete.call(name.clone());
                                }
                            },
                            {t.skills_delete_btn}
                        }
                    }
                }
            }
        }

        if clear_confirm_open() {
            Modal {
                title: t.skills_confirm_clear_all_title.to_string(),
                on_close: move |_| clear_confirm_open.set(false),
                div { class: "ains-skills__confirm",
                    p { {t.skills_confirm_clear_all_msg} }
                    div { class: "ains-skills__confirm-actions",
                        button {
                            class: "ains-skills__btn ains-skills__btn--cancel",
                            r#type: "button",
                            onclick: move |_| clear_confirm_open.set(false),
                            {t.modal_close}
                        }
                        button {
                            class: "ains-skills__btn ains-skills__btn--danger",
                            r#type: "button",
                            onclick: move |_| {
                                clear_confirm_open.set(false);
                                on_clear_all.call(());
                            },
                            {t.skills_clear_all_btn}
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SkillCardRow(
    card: SkillCard,
    on_open: EventHandler<String>,
    on_delete_request: EventHandler<String>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    let (trust_variant, trust_label) = match card.trust {
        SkillTrustView::System => (BadgeVariant::Admin, t.skills_trust_system),
        SkillTrustView::Trusted => (BadgeVariant::Success, t.skills_trust_trusted),
        SkillTrustView::Generated => (BadgeVariant::User, t.skills_trust_generated),
        SkillTrustView::Temporary => (BadgeVariant::Warning, t.skills_trust_temporary),
    };

    let name_for_open = card.name.clone();
    let name_for_delete = card.name.clone();
    let corrupted = card.corrupted;
    let system = card.trust == SkillTrustView::System;
    rsx! {
        div { class: if corrupted { "ains-skills__card ains-skills__card--corrupted" } else { "ains-skills__card" },
            button {
                class: "ains-skills__card-main",
                r#type: "button",
                disabled: corrupted,
                onclick: move |_| {
                    if !corrupted {
                        on_open.call(name_for_open.clone());
                    }
                },
                div { class: "ains-skills__card-head",
                    span { class: "ains-skills__card-name", "{card.name}" }
                    Badge { variant: trust_variant, {trust_label} }
                }
                if corrupted {
                    p { class: "ains-skills__card-desc ains-skills__card-desc--corrupted",
                        {t.skills_corrupted}
                    }
                } else {
                    p { class: "ains-skills__card-desc", "{card.description}" }
                }
                div { class: "ains-skills__card-meta",
                    span { "{t.skills_column_category}: {card.category}" }
                    span { "{t.skills_column_created}: {card.created_at}" }
                    if !card.requires_tools.is_empty() {
                        span { "{t.skills_requires_tools}: {card.requires_tools.join(\", \")}" }
                    }
                }
            }
            if !system {
                button {
                    class: "ains-skills__delete",
                    r#type: "button",
                    aria_label: t.skills_delete_btn,
                    onclick: move |_| on_delete_request.call(name_for_delete.clone()),
                    Trash2 {}
                }
            }
        }
    }
}
