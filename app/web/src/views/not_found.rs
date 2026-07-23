//! 404 兜底视图 —— catch-all 路由。
//!
//! 样式、结构、字体与 `/forgot-password`、`/reset-password` 鉴权页完全对齐：
//! 玻璃卡片 + 装饰光晕 + 系统默认 Logo（favicon.jpg）。
//! "返回主页" 指向根路由 `/`（对已登录/未登录用户均通用）。

use dioxus::prelude::*;

use ui::I18nContext;

use crate::Route;

/// 系统默认 Logo（与浏览器 favicon 同源），保持与鉴权页品牌一致。
const LOGO: Asset = asset!("/assets/favicon.jpg");

#[component]
pub fn NotFound(route: Vec<String>) -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    let nav = use_navigator();
    let _ = route; // catch-all 段保留以满足 Routable trait
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/not_found.css") }
        div { class: "ains-notfound",
            div { class: "ains-notfound__orb ains-notfound__orb--blue" }
            div { class: "ains-notfound__orb ains-notfound__orb--indigo" }

            div { class: "ains-notfound__card",
                img { class: "ains-notfound__icon", src: LOGO, alt: "AINS" }
                h1 { class: "ains-notfound__title", "404" }
                p { class: "ains-notfound__subtitle", {t.not_found_page} }

                div { class: "ains-notfound__back-row",
                    a {
                        class: "ains-notfound__back",
                        href: "#",
                        onclick: move |e| {
                            e.prevent_default();
                            nav.push(Route::LoginLanding {});
                        },
                        {t.not_found_back_to_home}
                    }
                }
            }
        }
    }
}
