use dioxus::prelude::*;
use dioxus_icons::lucide::NotebookPen;

use crate::{Badge, BadgeVariant, EN, I18nContext, Modal};

/// 权限模式（视图模型，与 rust-agent `PermissionMode` 三态一一对应，
/// 宿主负责双向映射；ui 层不依赖 rust-agent）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionModeView {
    #[default]
    Default,
    Plan,
    FullAuto,
}

/// 权限模式切换器 —— default / plan / full_auto 三态（Phase 6.11）。
///
/// full_auto 属放宽安全边界的方向，切换前弹出模态二次确认。
#[component]
pub fn PermissionModeSwitcher(
    mode: PermissionModeView,
    on_change: EventHandler<PermissionModeView>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    // full_auto 待确认状态：Some 表示确认条展开。
    let mut pending_full_auto = use_signal(|| false);

    let seg_class = |active: bool| {
        if active {
            "ains-perm-mode__seg ains-perm-mode__seg--active"
        } else {
            "ains-perm-mode__seg"
        }
    };

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/permission.css") }
        div { class: "ains-perm-mode",
            span { class: "ains-perm-mode__label", {t.perm_mode_label} }
            div { class: "ains-perm-mode__group", role: "group",
                button {
                    class: seg_class(mode == PermissionModeView::Default),
                    r#type: "button",
                    onclick: move |_| {
                        pending_full_auto.set(false);
                        on_change.call(PermissionModeView::Default);
                    },
                    {t.perm_mode_default}
                }
                button {
                    class: seg_class(mode == PermissionModeView::Plan),
                    r#type: "button",
                    onclick: move |_| {
                        pending_full_auto.set(false);
                        on_change.call(PermissionModeView::Plan);
                    },
                    {t.perm_mode_plan}
                }
                button {
                    class: seg_class(mode == PermissionModeView::FullAuto),
                    r#type: "button",
                    onclick: move |_| {
                        if mode != PermissionModeView::FullAuto {
                            pending_full_auto.set(true);
                        }
                    },
                    {t.perm_mode_full_auto}
                }
            }
        }
        if pending_full_auto() {
            Modal {
                title: t.perm_mode_full_auto_confirm_title.to_string(),
                on_close: move |_| pending_full_auto.set(false),
                hide_close: true,
                div { class: "ains-perm-mode__confirm-body",
                    p { class: "ains-perm-mode__confirm-msg", {t.perm_mode_full_auto_confirm_msg} }
                    div { class: "ains-perm-mode__confirm-actions",
                        button {
                            class: "ains-perm__btn ains-perm__btn--secondary",
                            r#type: "button",
                            onclick: move |_| pending_full_auto.set(false),
                            {t.cancel_label}
                        }
                        button {
                            class: "ains-perm__btn ains-perm__btn--allow",
                            r#type: "button",
                            onclick: move |_| {
                                pending_full_auto.set(false);
                                on_change.call(PermissionModeView::FullAuto);
                            },
                            {t.perm_mode_full_auto_confirm_btn}
                        }
                    }
                }
            }
        }
    }
}

/// Plan Mode 常驻指示徽标（Phase 6.11）。非 Plan 模式渲染为空。
#[component]
pub fn PlanModeIndicator(mode: PermissionModeView) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    if mode != PermissionModeView::Plan {
        return rsx! {};
    }
    rsx! {
        Badge { variant: BadgeVariant::Warning,
            NotebookPen { class: "ains-perm-mode__plan-icon" }
            {t.perm_plan_indicator}
        }
    }
}
