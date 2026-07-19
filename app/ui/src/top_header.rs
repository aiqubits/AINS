use dioxus::prelude::*;
use dioxus_icons::lucide::{ChevronDown, LogOut, Menu, Search, Settings};

use crate::{I18nContext, LanguageSwitcher, LanguageSwitcherVariant};

/// TopHeader —— 80px sticky 顶栏。
///
/// 按 DESIGN.md §3.3 规格。
/// 用户区域点击后弹出下拉菜单，包含「个人设置」和「登出」两项。
#[component]
pub fn TopHeader(
    on_sidebar_toggle: EventHandler<MouseEvent>,
    search_value: Signal<String>,
    user_name: String,
    user_email: String,
    /// 点击「个人设置」时触发，通常用于跳转到设置页。
    #[props(default)]
    on_settings_click: Option<EventHandler<MouseEvent>>,
    /// 点击「登出」时触发，通常由调用方弹出确认框后执行登出。
    #[props(default)]
    on_logout: Option<EventHandler<MouseEvent>>,
) -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    let mut dropdown_open = use_signal(|| false);

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/top_header.css") }
        header { class: "ains-top-header",
            // 左侧：汉堡菜单 + 搜索框
            div { class: "ains-top-header__left",
                button {
                    class: "ains-top-header__hamburger",
                    onclick: move |e| on_sidebar_toggle.call(e),
                    Menu {}
                }
                div { class: "ains-top-header__search",
                    Search { class: "ains-top-header__search-icon" }
                    input {
                        class: "ains-top-header__search-input",
                        r#type: "text",
                        placeholder: t.top_header_search_placeholder,
                        value: search_value,
                        oninput: move |e| *search_value.write() = e.value(),
                    }
                }
            }

            // 右侧：语言切换 + 状态 + 用户下拉菜单
            div { class: "ains-top-header__right",
                // 语言切换单按钮
                LanguageSwitcher { variant: LanguageSwitcherVariant::Header }

                span { class: "ains-top-header__status",
                    span { class: "ains-top-header__status-dot" }
                    span { class: "ains-top-header__status-text", {t.top_header_node_online} }
                }

                // 用户下拉菜单容器
                div { class: "ains-top-header__user-menu",
                    // 触发区：头像 + 身份
                    div {
                        class: "ains-top-header__user ains-top-header__user--clickable",
                        title: t.top_header_click_to_expand,
                        onclick: move |_| dropdown_open.toggle(),
                        div { class: "ains-top-header__avatar", "WS" }
                        div { class: "ains-top-header__identity",
                            span { class: "ains-top-header__name", "{user_name}" }
                            span { class: "ains-top-header__email", "{user_email}" }
                        }
                        ChevronDown { class: "ains-top-header__user-chevron" }
                    }

                    // 下拉菜单
                    if dropdown_open() {
                        // 全屏透明遮罩 —— 点击非菜单区域关闭下拉菜单
                        div {
                            class: "ains-top-header__dropdown-overlay",
                            onclick: move |_| dropdown_open.set(false),
                        }
                        div { class: "ains-top-header__dropdown",
                            // 菜单项：个人设置
                            button {
                                class: "ains-top-header__dropdown-item",
                                r#type: "button",
                                onclick: move |e| {
                                    dropdown_open.set(false);
                                    if let Some(ref h) = on_settings_click {
                                        h.call(e);
                                    }
                                },
                                Settings { class: "ains-top-header__dropdown-icon" }
                                span { {t.top_header_settings_label} }
                            }
                            // 分隔线
                            div { class: "ains-top-header__dropdown-divider" }
                            // 菜单项：登出
                            button {
                                class: "ains-top-header__dropdown-item ains-top-header__dropdown-item--danger",
                                r#type: "button",
                                onclick: move |e| {
                                    dropdown_open.set(false);
                                    if let Some(ref h) = on_logout {
                                        h.call(e);
                                    }
                                },
                                LogOut { class: "ains-top-header__dropdown-icon" }
                                span { {t.top_header_logout_label} }
                            }
                        }
                    }
                }
            }
        }
    }
}
