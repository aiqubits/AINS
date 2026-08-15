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
const LOGIN_VIEW: &str = include_str!("../src/views/login_landing.rs");

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

/// 注册/邮箱验证后的手动登录应始终提供验证码输入；只有确认启用时才在
/// 前端强制填写。能力状态未知时保持中性提示，不能误导用户系统已开启验证码。
#[test]
fn manual_login_notice_distinguishes_confirmed_and_unknown_captcha_status() {
    assert!(
        LOGIN_VIEW.contains("let mut manual_captcha_input = use_signal(|| false);"),
        "登录页需要保留手动验证码输入状态"
    );
    assert!(
        LOGIN_VIEW.contains("let mut manual_captcha_required = use_signal(|| false);"),
        "登录页需要区分验证码展示与必填状态"
    );
    assert!(
        LOGIN_VIEW.contains("manual_login_captcha_policy(notice)"),
        "登录页必须使用经过单测的手动登录验证码策略"
    );
    assert!(
        LOGIN_VIEW.contains("*wechat_enabled.read() || *manual_captcha_input.read()"),
        "能力状态未知时也必须展示验证码输入"
    );
    assert!(
        LOGIN_VIEW.contains("*wechat_enabled.read() || *manual_captcha_required.read()"),
        "仅确认启用时才在前端强制填写验证码"
    );
    assert!(
        LOGIN_VIEW.contains("auth_register_manual_login_status_unknown"),
        "能力状态未知时必须显示中性提示，不能宣称微信验证码已启用"
    );
}
