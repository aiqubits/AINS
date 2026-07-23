//! 公开鉴权页 / 404 页的跳转目标回归测试。
//!
//! 防护两类此前修复过的缺陷：
//! 1. **跳转到 admin-only 的 `Dashboard`**：`/dashboard` 位于 `RequireAdmin` layout 之下
//!    （见 `app/web/src/main.rs`），非管理员的已登录用户被跳到该路由会再次被弹开。
//!    公开鉴权页（forgot / reset / verify）的“已登录守卫”必须跳到对所有已登录用户
//!    都可达的 `PersonalCenter`，而不是 `Dashboard`。
//! 2. **404 页指向受保护路由**：`not_found.rs` 早期的“返回”链接指向 `PersonalCenter`
//!    （受保护路由），未登录用户点击会被弹到登录页。现要求指向公开根路由 `LoginLanding`。
//!
//! 与 `auth_view_styles_test.rs` 一致：纯 std + `include_str!`，编译期静态校验，无需 VirtualDom。

const FORGOT_VIEW: &str = include_str!("../src/views/forgot_password.rs");
const RESET_VIEW: &str = include_str!("../src/views/reset_password.rs");
const VERIFY_VIEW: &str = include_str!("../src/views/verify_email.rs");
const NOT_FOUND_VIEW: &str = include_str!("../src/views/not_found.rs");

/// 需要“已登录守卫 → PersonalCenter”的公开鉴权页。
const AUTH_GUARD_VIEWS: [(&str, &str); 3] = [
    ("forgot_password.rs", FORGOT_VIEW),
    ("reset_password.rs", RESET_VIEW),
    ("verify_email.rs", VERIFY_VIEW),
];

/// 所有公开、可被未登录用户访问的视图（含 404），均不得引用受保护路由。
const PUBLIC_VIEWS: [(&str, &str); 4] = [
    ("forgot_password.rs", FORGOT_VIEW),
    ("reset_password.rs", RESET_VIEW),
    ("verify_email.rs", VERIFY_VIEW),
    ("not_found.rs", NOT_FOUND_VIEW),
];

/// 公开视图不得跳转到 admin-only 的 `Dashboard`（会把非管理员已登录用户再次弹开）。
#[test]
fn public_views_never_redirect_to_admin_dashboard() {
    for (name, src) in PUBLIC_VIEWS {
        assert!(
            !src.contains("Route::Dashboard"),
            "{name} 不应跳转到 admin-only 的 Route::Dashboard；\
             已登录守卫应使用对所有已登录用户可达的 Route::PersonalCenter"
        );
    }
}

/// forgot / reset / verify 的“已登录守卫”必须跳到 PersonalCenter。
#[test]
fn auth_guards_redirect_to_personal_center() {
    for (name, src) in AUTH_GUARD_VIEWS {
        assert!(
            src.contains("Route::PersonalCenter {}"),
            "{name} 的已登录守卫必须 nav.replace(Route::PersonalCenter {{}})"
        );
    }
}

/// 404 页“返回”只能指向公开根路由 LoginLanding，且不得引用受保护的 PersonalCenter，
/// 否则未登录用户点击会被弹到登录页。
#[test]
fn not_found_back_targets_public_root_only() {
    assert!(
        NOT_FOUND_VIEW.contains("Route::LoginLanding {}"),
        "not_found.rs 的返回链接必须指向公开根路由 Route::LoginLanding"
    );
    assert!(
        !NOT_FOUND_VIEW.contains("Route::PersonalCenter"),
        "not_found.rs 不应指向受保护的 Route::PersonalCenter（未登录用户会被弹开）"
    );
}
