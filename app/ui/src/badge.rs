use dioxus::prelude::*;

/// Badge —— 标签 / 角色标识。
///
/// 按 DESIGN.md §3.5 规格提供 5 种变体。
#[component]
pub fn Badge(
    children: Element,
    #[props(default = BadgeVariant::User)] variant: BadgeVariant,
) -> Element {
    let class = match variant {
        BadgeVariant::Success => "ains-badge ains-badge--success",
        BadgeVariant::Warning => "ains-badge ains-badge--warning",
        BadgeVariant::Admin => "ains-badge ains-badge--admin",
        BadgeVariant::User => "ains-badge ains-badge--user",
        BadgeVariant::AmberCompact => "ains-badge ains-badge--amber-compact",
    };

    rsx! {
        document::Link {
            rel: "stylesheet",
            href: asset!("/assets/styling/badge.css"),
        }
        span { class: class, {children} }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeVariant {
    Success,
    Warning,
    /// 紫色软底 — 管理员角色
    Admin,
    #[default]
    /// 灰色中性 — 普通用户角色
    User,
    /// 9px 紧凑版 — 仅用于 Sidebar 中的 `admin_layer` 标签
    AmberCompact,
}
