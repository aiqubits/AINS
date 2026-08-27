//! 宽屏管理表格使用更宽的共享内容区，其他页面保持默认阅读宽度。

const APP_SHELL_CSS: &str = include_str!("../../ui/assets/styling/app_shell.css");
#[cfg(target_arch = "wasm32")]
const DATA_TABLE_CSS: &str = include_str!("../../ui/assets/styling/data_table.css");
const MAIN_CSS: &str = include_str!("../assets/main.css");
const WEB_MANIFEST: &str = include_str!("../Cargo.toml");
const CI_WORKFLOW: &str = include_str!("../../../.github/workflows/ains.yml");

fn css_rule<'a>(stylesheet: &'a str, selector: &str) -> &'a str {
    let marker = format!("{selector} {{");
    stylesheet
        .split_once(&marker)
        .unwrap_or_else(|| panic!("stylesheet must define {selector}"))
        .1
        .split_once('}')
        .unwrap_or_else(|| panic!("{selector} must be a complete CSS rule"))
        .0
}

#[test]
fn management_content_modifier_expands_the_shell_width() {
    let rule = css_rule(APP_SHELL_CSS, ".ains-app-shell__content--wide-management");

    assert!(
        rule.contains("max-width: none"),
        "management pages should use the full available desktop viewport"
    );
}

#[test]
fn narrow_management_tables_keep_the_horizontal_scroll_escape_hatch() {
    let content_rule = css_rule(APP_SHELL_CSS, ".ains-app-shell__content");
    assert!(content_rule.contains("width: 100%"));

    let wrapper_rule = css_rule(MAIN_CSS, ".ains-users__table-wrapper");
    assert!(wrapper_rule.contains("overflow-x: auto"));

    let table_container_rule = css_rule(MAIN_CSS, ".ains-users__table-wrapper .ains-table");
    assert!(table_container_rule.contains("width: max-content"));
    assert!(table_container_rule.contains("min-width: 100%"));
}

#[test]
fn browser_layout_regression_has_its_own_wasm_feature_and_ci_runner() {
    let wasm_dev_dependencies = WEB_MANIFEST
        .split_once("[target.'cfg(target_arch = \"wasm32\")'.dev-dependencies]")
        .expect("web manifest must define wasm32 dev-dependencies")
        .1
        .split_once("\n[features]")
        .expect("web manifest must keep the features section after dev-dependencies")
        .0;
    assert!(
        wasm_dev_dependencies.contains("\"CssStyleDeclaration\""),
        "the browser test must not rely on another workspace package to enable web-sys/CssStyleDeclaration"
    );
    assert!(
        CI_WORKFLOW.contains("wasm-pack test --headless --chrome app/web"),
        "CI must execute app/web wasm_bindgen_test coverage in a real browser"
    );
}

#[cfg(target_arch = "wasm32")]
mod browser_tests {
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::{Element, Window};

    use super::{APP_SHELL_CSS, DATA_TABLE_CSS, MAIN_CSS};

    wasm_bindgen_test_configure!(run_in_browser);

    struct BrowserFixture(Element);

    impl Drop for BrowserFixture {
        fn drop(&mut self) {
            self.0.remove();
        }
    }

    fn computed_property(window: &Window, element: &Element, property: &str) -> String {
        window
            .get_computed_style(element)
            .expect("read computed style")
            .expect("element has computed style")
            .get_property_value(property)
            .unwrap_or_else(|_| panic!("read computed {property}"))
    }

    #[wasm_bindgen_test]
    fn widest_management_table_aligns_cells_and_scrolls_on_a_narrow_surface() {
        let window = web_sys::window().expect("browser window");
        let document = window.document().expect("browser document");
        let root = document.create_element("div").expect("test root");
        let _fixture = BrowserFixture(root.clone());
        root.set_attribute("style", "width:320px")
            .expect("constrain test root");

        let stylesheet = document.create_element("style").expect("style element");
        stylesheet.set_text_content(Some(&format!(
            "{MAIN_CSS}\n{APP_SHELL_CSS}\n{DATA_TABLE_CSS}"
        )));
        root.append_child(&stylesheet).expect("attach styles");

        let markup = document.create_element("div").expect("test markup");
        markup.set_inner_html(
            r#"
            <main id="management-content" class="ains-app-shell__content ains-app-shell__content--wide-management">
              <div id="management-wrapper" class="ains-users__table-wrapper">
                <div class="ains-table">
                  <table class="ains-table__table" style="--ains-table-col-1-align:left;--ains-table-col-2-align:left;--ains-table-col-3-align:center;--ains-table-col-4-align:left;--ains-table-col-5-align:center;--ains-table-col-6-align:right;--ains-table-col-7-align:left;--ains-table-col-8-align:left;--ains-table-col-9-align:center;">
                    <thead class="ains-table__head"><tr>
                      <th class="ains-table__th">Name</th><th class="ains-table__th w-40">Tenant</th>
                      <th class="ains-table__th w-24">Protocol</th><th class="ains-table__th w-48">Base URL</th>
                      <th class="ains-table__th w-20">Status</th><th class="ains-table__th w-16">Weight</th>
                      <th class="ains-table__th w-48">Capabilities</th><th class="ains-table__th w-36">Created</th>
                      <th class="ains-table__th w-36">Actions</th>
                    </tr></thead>
                    <tbody class="ains-table__body"><tr>
                      <td>Name</td><td>Tenant</td><td>Protocol</td><td>URL</td><td>Status</td>
                      <td id="numeric-cell">1</td><td>chat</td><td>2026-08-27</td><td id="action-cell">Edit</td>
                    </tr></tbody>
                  </table>
                </div>
              </div>
            </main>
            "#,
        );
        root.append_child(&markup).expect("attach markup");
        document
            .body()
            .expect("document body")
            .append_child(&root)
            .expect("attach test root");

        let content = document
            .get_element_by_id("management-content")
            .expect("management content");
        let wrapper = document
            .get_element_by_id("management-wrapper")
            .expect("management wrapper");
        let numeric_cell = document
            .get_element_by_id("numeric-cell")
            .expect("numeric cell");
        let action_cell = document
            .get_element_by_id("action-cell")
            .expect("action cell");

        assert_eq!(computed_property(&window, &content, "max-width"), "none");
        assert_eq!(computed_property(&window, &wrapper, "overflow-x"), "auto");
        assert_eq!(
            computed_property(&window, &numeric_cell, "text-align"),
            "right"
        );
        assert_eq!(
            computed_property(&window, &action_cell, "text-align"),
            "center"
        );
        assert!(
            wrapper.scroll_width() > wrapper.client_width(),
            "the widest management table should remain horizontally reachable on a narrow surface"
        );
    }
}
