//! Modal / Button 基础样式的首屏预加载回归测试。
//!
//! Modal 通常仅会在用户首次点击后才挂载。若把基础样式 link 放在 Modal
//! 内部，首次打开时 DOM 会先于 CSS 绘制；若在 CSS 中以固定 URL `@import`
//! 另一个由 `asset!` 管理的资源，则发布构建的带 hash 路径无法被正确引用。

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::document::{Document, Eval, LinkProps, NoOpDocument};
use dioxus::prelude::*;
use dioxus_core::ScopeId;

const GLOBAL_STYLES: &str = include_str!("../../ui/src/global_styles.rs");
const CONFIRM_DIALOG_CSS: &str = include_str!("../assets/components/confirm_dialog.css");
const WEB_APP: &str = include_str!("../src/main.rs");
const DESKTOP_APP: &str = include_str!("../../desktop/src/main.rs");
const MOBILE_APP: &str = include_str!("../../mobile/src/main.rs");

/// 测试用 Document：捕获 Dioxus 在首轮渲染中真正注册到 document head 的 link。
#[derive(Default)]
struct CapturingDocument {
    stylesheet_hrefs: RefCell<Vec<String>>,
}

impl Document for CapturingDocument {
    fn eval(&self, js: String) -> Eval {
        NoOpDocument.eval(js)
    }

    fn create_link(&self, props: LinkProps) {
        if props.rel.as_deref() == Some("stylesheet")
            && let Some(href) = props.href
        {
            self.stylesheet_hrefs.borrow_mut().push(href);
        }
    }
}

#[component]
fn GlobalStylesTestApp() -> Element {
    rsx! {
        ui::GlobalStyles {}
    }
}

#[test]
fn global_styles_preload_modal_and_button_primitives() {
    for asset in [
        "/assets/styling/tokens.css",
        "/assets/styling/button.css",
        "/assets/styling/modal.css",
    ] {
        assert!(
            GLOBAL_STYLES.contains(&format!("asset!(\"{asset}\")")),
            "GlobalStyles 必须在首屏通过 asset! 预加载 {asset}，避免首次交互出现无样式组件"
        );
    }
}

#[test]
fn global_styles_registers_primitives_in_the_document_head_on_first_render() {
    let document = Rc::new(CapturingDocument::default());
    let document_context: Rc<dyn Document> = document.clone();
    let mut dom = VirtualDom::new(GlobalStylesTestApp);
    dom.in_scope(ScopeId::ROOT, || provide_context(document_context));
    dom.rebuild_in_place();

    let links = document.stylesheet_hrefs.borrow();
    for asset_name in ["tokens.css", "button.css", "modal.css"] {
        assert!(
            links.iter().any(|href| href.ends_with(asset_name)),
            "GlobalStyles 首轮渲染必须将 {asset_name} 注册到 document head；实际链接：{links:?}"
        );
    }
}

#[test]
fn every_application_root_mounts_global_styles() {
    for (platform, source) in [
        ("web", WEB_APP),
        ("desktop", DESKTOP_APP),
        ("mobile", MOBILE_APP),
    ] {
        assert!(
            source.contains("ui::GlobalStyles {}"),
            "{platform} 根组件必须挂载 ui::GlobalStyles，否则基础 Modal/Button CSS 不会在首轮渲染预加载"
        );
    }
}

#[test]
fn confirm_dialog_does_not_bypass_hashed_button_asset() {
    assert!(
        !CONFIRM_DIALOG_CSS.contains("@import"),
        "ConfirmDialog 不得用固定 URL @import button.css；应由 GlobalStyles 的 asset! 链接加载"
    );
}
