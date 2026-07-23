//! Auth 视图与 404 页样式一致性回归测试。
//!
//! 防护此前的缺陷：`forgot_password.rs` / `reset_password.rs` 使用了大量
//! `ains-forgot*` / `ains-reset*` BEM 类，但既没有任何 CSS 文件定义这些类，
//! 视图本身也没有 `document::Link` 引入样式表 —— 导致页面完全无样式
//! （卡片消失、按钮通栏铺满）。404 页（`not_found.rs`）后续与鉴权页对齐，
//! 一并纳入校验。
//!
//! 本测试在**编译期**通过 `include_str!` 读取视图源码与对应 CSS，静态校验：
//! 1. 视图中出现的每个 `ains-<prefix>*` 类都能在 CSS 中找到同名选择器；
//! 2. 视图确实通过 `asset!(...)` 引入了对应的样式表；
//! 3. 鉴权页与 404 页顶部图标均引用系统默认 Logo（favicon.jpg）。
//!
//! 纯 std 实现，无需 VirtualDom。

use std::collections::BTreeSet;

/// 从视图源码提取所有以 `prefix` 开头的 class token。
fn extract_class_tokens(source: &str, prefix: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = source[search_from..].find(prefix) {
        let start = search_from + rel;
        // 读取从 start 起的最长 class token（BEM 允许的字符：字母、数字、`-`、`_`）。
        let mut end = start;
        while end < bytes.len() {
            let c = bytes[end] as char;
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                end += 1;
            } else {
                break;
            }
        }
        tokens.insert(source[start..end].to_string());
        search_from = end;
    }
    tokens
}

/// CSS 中是否存在 `.token` 作为一个完整选择器（其后须为选择器边界字符，
/// 避免把 `.ains-forgot` 误判为已由 `.ains-forgot__orb` 定义）。
fn css_defines_class(css: &str, token: &str) -> bool {
    let needle = format!(".{token}");
    let bytes = css.as_bytes();
    let mut from = 0;
    while let Some(rel) = css[from..].find(&needle) {
        let pos = from + rel;
        let after = pos + needle.len();
        let boundary = match bytes.get(after) {
            None => true,
            Some(&b) => {
                let c = b as char;
                !(c.is_ascii_alphanumeric() || c == '-' || c == '_')
            }
        };
        if boundary {
            return true;
        }
        from = after;
    }
    false
}

fn assert_all_classes_defined(view_src: &str, css: &str, prefix: &str) {
    let tokens = extract_class_tokens(view_src, prefix);
    assert!(
        !tokens.is_empty(),
        "预期视图中至少出现一个 `{prefix}` 类，提取结果为空说明用例失效"
    );
    let missing: Vec<&String> = tokens
        .iter()
        .filter(|t| !css_defines_class(css, t))
        .collect();
    assert!(
        missing.is_empty(),
        "以下 `{prefix}` 类在视图中被引用，但对应 CSS 未定义（会导致页面无样式）：{missing:?}"
    );
}

const FORGOT_VIEW: &str = include_str!("../src/views/forgot_password.rs");
const FORGOT_CSS: &str = include_str!("../assets/styling/forgot_password.css");
const RESET_VIEW: &str = include_str!("../src/views/reset_password.rs");
const RESET_CSS: &str = include_str!("../assets/styling/reset_password.css");
const NOT_FOUND_VIEW: &str = include_str!("../src/views/not_found.rs");
const NOT_FOUND_CSS: &str = include_str!("../assets/styling/not_found.css");
const VERIFY_VIEW: &str = include_str!("../src/views/verify_email.rs");
const VERIFY_CSS: &str = include_str!("../assets/styling/verify_email.css");

/// forgot-password 视图使用的每个 `ains-forgot*` 类都必须在 CSS 中定义。
#[test]
fn forgot_password_classes_all_have_css() {
    assert_all_classes_defined(FORGOT_VIEW, FORGOT_CSS, "ains-forgot");
}

/// reset-password 视图使用的每个 `ains-reset*` 类都必须在 CSS 中定义。
#[test]
fn reset_password_classes_all_have_css() {
    assert_all_classes_defined(RESET_VIEW, RESET_CSS, "ains-reset");
}

/// 404 视图使用的每个 `ains-notfound*` 类都必须在 CSS 中定义。
#[test]
fn not_found_classes_all_have_css() {
    assert_all_classes_defined(NOT_FOUND_VIEW, NOT_FOUND_CSS, "ains-notfound");
}

/// verify-email 视图使用的每个 `ains-verify*` 类都必须在 CSS 中定义。
#[test]
fn verify_email_classes_all_have_css() {
    assert_all_classes_defined(VERIFY_VIEW, VERIFY_CSS, "ains-verify");
}

/// 视图必须通过 `asset!(...)` 引入各自的样式表，否则 CSS 不会被打包/加载。
#[test]
fn auth_views_link_their_stylesheets() {
    assert!(
        FORGOT_VIEW.contains("asset!(\"/assets/styling/forgot_password.css\")"),
        "forgot_password.rs 必须通过 document::Link 引入 forgot_password.css"
    );
    assert!(
        RESET_VIEW.contains("asset!(\"/assets/styling/reset_password.css\")"),
        "reset_password.rs 必须通过 document::Link 引入 reset_password.css"
    );
    assert!(
        NOT_FOUND_VIEW.contains("asset!(\"/assets/styling/not_found.css\")"),
        "not_found.rs 必须通过 document::Link 引入 not_found.css"
    );
    assert!(
        VERIFY_VIEW.contains("asset!(\"/assets/styling/verify_email.css\")"),
        "verify_email.rs 必须通过 document::Link 引入 verify_email.css"
    );
}

/// 鉴权页与 404 页均使用系统默认 Logo（favicon.jpg），保持品牌对齐。
#[test]
fn brand_views_use_default_logo() {
    for (name, src) in [
        ("forgot_password.rs", FORGOT_VIEW),
        ("reset_password.rs", RESET_VIEW),
        ("not_found.rs", NOT_FOUND_VIEW),
        ("verify_email.rs", VERIFY_VIEW),
    ] {
        assert!(
            src.contains("asset!(\"/assets/favicon.jpg\")"),
            "{name} 顶部图标必须使用系统默认 Logo favicon.jpg"
        );
    }
}

/// 校验测试辅助函数：`.ains-forgot` 不应被 `.ains-forgot__orb` 的定义误判为已定义。
#[test]
fn css_defines_class_respects_selector_boundary() {
    let css = ".ains-forgot__orb { color: red; }";
    assert!(css_defines_class(css, "ains-forgot__orb"));
    assert!(
        !css_defines_class(css, "ains-forgot"),
        "`.ains-forgot` 未单独定义时不应被 `.ains-forgot__orb` 命中"
    );
}
