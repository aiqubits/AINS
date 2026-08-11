use dioxus::prelude::*;

/// 全局基础样式注入。
///
/// `tokens.css`、`button.css` 与 `modal.css` 必须在首屏加载：按钮和模态框
/// 都可能由用户的首次操作才挂载，若此时才插入 stylesheet link，浏览器会先
/// 绘制未样式化的 DOM（FOUC）。其它布局/业务组件的样式仍由组件就近加载。
///
/// 该组件挂在 web crate 的 `App` 根节点上，确保上述基础样式在任意路由切换或
/// 首次交互前已生效。
#[component]
pub fn GlobalStyles() -> Element {
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/tokens.css") }
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/button.css") }
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/modal.css") }
    }
}
