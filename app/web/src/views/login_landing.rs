//! LoginLanding 视图 —— `/` 根路由。
//!
//! 左侧为登录/注册表单（复用 AuthForm），右侧展示公众号二维码、版权声明与 GitHub 项目地址。
//! 已登录用户自动跳转到 `/dashboard`。

use dioxus::prelude::*;
use ui::{
    AuthForm, AuthMode, AuthPayload, I18nContext, LanguageSwitcher, LanguageSwitcherVariant,
    Translations,
};

use crate::Route;
use crate::api::{ErrorContext, humanize_error};
use crate::auth::{AuthState, ManualLoginNotice, RegisterOutcome, manual_login_captcha_policy};
use crate::components::{HttpMethod, LogBus, push_log_result};

const QRCODE_IMG: Asset = asset!("/assets/qrcode-op.jpg");

/// 提交表单后的导航动作。
enum SubmitAction {
    Nothing,
    ReturnToLogin,
    NavigateToVerify { email: String },
}

/// 将语义化的手动登录提示按当前语言转换为展示文案。
///
/// 不把翻译后的字符串存入 signal，避免用户切换语言后仍看到旧语言的提示。
fn manual_login_notice_text(t: &Translations, notice: ManualLoginNotice) -> String {
    match notice {
        ManualLoginNotice::CaptchaRequired => t.auth_register_manual_login.to_string(),
        ManualLoginNotice::CaptchaStatusUnknown => {
            t.auth_register_manual_login_status_unknown.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus_core::{NoOpMutations, VirtualDom};
    use std::sync::atomic::{AtomicBool, Ordering};
    use ui::{EN, ZH};

    #[test]
    fn manual_login_notice_uses_the_current_translation_set() {
        assert_eq!(
            manual_login_notice_text(&EN, ManualLoginNotice::CaptchaRequired),
            EN.auth_register_manual_login
        );
        assert_eq!(
            manual_login_notice_text(&ZH, ManualLoginNotice::CaptchaRequired),
            ZH.auth_register_manual_login
        );
        assert_ne!(
            manual_login_notice_text(&EN, ManualLoginNotice::CaptchaRequired),
            manual_login_notice_text(&ZH, ManualLoginNotice::CaptchaRequired)
        );
        assert_eq!(
            manual_login_notice_text(&EN, ManualLoginNotice::CaptchaStatusUnknown),
            EN.auth_register_manual_login_status_unknown
        );
        assert_eq!(
            manual_login_notice_text(&ZH, ManualLoginNotice::CaptchaStatusUnknown),
            ZH.auth_register_manual_login_status_unknown
        );
    }

    /// 覆盖注册成功提示已经展示后，用户再切换语言的场景。
    #[test]
    fn displayed_manual_login_notice_rerenders_in_the_new_language() {
        static EN_NOTICE_RENDERED: AtomicBool = AtomicBool::new(false);
        static ZH_NOTICE_RENDERED: AtomicBool = AtomicBool::new(false);

        EN_NOTICE_RENDERED.store(false, Ordering::SeqCst);
        ZH_NOTICE_RENDERED.store(false, Ordering::SeqCst);

        let mut dom = VirtualDom::new(|| {
            use_context_provider(|| I18nContext::new(ui::Language::En));
            let i18n = use_context::<I18nContext>();
            // 模拟注册完成后已保留的提示状态；该状态跨语言切换保持不变。
            let manual_login_info = use_signal(|| Some(ManualLoginNotice::CaptchaRequired));
            let info = (*manual_login_info.read())
                .map(|notice| manual_login_notice_text(i18n.t(), notice));

            // 语言切换在提交后的 effect 中发生，模拟 LanguageSwitcher 的事件更新，
            // 而非在渲染过程中写入 signal。
            let mut switched = use_signal(|| false);
            let mut i18n_for_switch = i18n;
            use_effect(move || {
                if !*switched.read() {
                    switched.set(true);
                    i18n_for_switch.set_lang(ui::Language::Zh);
                }
            });

            match i18n.lang() {
                ui::Language::En => {
                    EN_NOTICE_RENDERED.store(
                        info.as_deref() == Some(EN.auth_register_manual_login),
                        Ordering::SeqCst,
                    );
                }
                ui::Language::Zh => {
                    ZH_NOTICE_RENDERED.store(
                        info.as_deref() == Some(ZH.auth_register_manual_login),
                        Ordering::SeqCst,
                    );
                }
            }

            rsx! { p { "{info:?}" } }
        });
        dom.rebuild_in_place();
        dom.render_immediate(&mut NoOpMutations);
        dom.render_immediate(&mut NoOpMutations);

        assert!(
            EN_NOTICE_RENDERED.load(Ordering::SeqCst),
            "提示首次显示时应使用英文"
        );
        assert!(
            ZH_NOTICE_RENDERED.load(Ordering::SeqCst),
            "切换语言后，已显示的提示应重新渲染为中文"
        );
    }
}

#[component]
pub fn LoginLanding() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    let auth = use_context::<AuthState>();
    let log_bus = use_context::<LogBus>();
    let nav = use_navigator();

    // 等待 AuthState 初始化完成，避免「记住登录」用户首屏被误判为未登录
    // 在 restore_from_storage_async 进行中（initialized=false），user 为 None，
    // 如果不检查 initialized 会导致已登录用户短暂看到登录表单
    let is_initialized = *auth.initialized.read();
    if !is_initialized {
        return rsx! {
            Fragment {}
        };
    }

    // 进入登录页时清空 pending 残留
    let mut auth_for_clear = auth.clone();
    use_effect(move || {
        auth_for_clear.clear_pending_registration();
    });

    // 已登录则直接跳到个人中心（所有角色的默认落地页）
    let authenticated_at_render = auth.is_authenticated();
    let auth_for_effect = auth.clone();
    use_effect(move || {
        // 显式读取 auth.user 以建立响应式依赖
        // 当登录成功后 auth.user 变化时，此 effect 会重新执行并触发导航
        if auth_for_effect.is_authenticated() {
            nav.replace(Route::PersonalCenter {});
        }
    });

    // 渲染时前置判断：已登录时不渲染表单，消除首帧闪现
    if authenticated_at_render {
        return rsx! {
            Fragment {}
        };
    }

    let mode = use_signal(AuthMode::default);
    let mut name = use_signal(String::new);
    let mut email = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut password_confirm = use_signal(String::new);
    let mut captcha_code = use_signal(String::new);
    let remember = use_signal(|| false);
    let mut loading = use_signal(|| false);
    let mut error_msg = use_signal(|| Option::<String>::None);
    // 保留提示的语义状态；实际文案在渲染时按当前 i18n 语言生成。
    let mut manual_login_info = use_signal(|| Option::<ManualLoginNotice>::None);
    let mut wechat_enabled = use_signal(|| false);
    // 若注册后的能力查询失败，仍需让用户能够输入验证码完成登录。未知状态下
    // 输入框仅展示、不在前端强制必填；确认启用时才强制填写。
    // 这些状态仅在本次登录页生命周期内有效，避免影响之后的普通登录。
    let mut manual_captcha_input = use_signal(|| false);
    let mut manual_captcha_required = use_signal(|| false);

    // 注册或邮箱验证完成后，AuthState 置位一次性通知；登录页消费后展示
    // 成功反馈。该信号订阅确保本页内从“注册”切回“登录”时也能立即显示。
    let mut auth_for_manual_login_notice = auth.clone();
    use_effect(move || {
        let notice = { *auth_for_manual_login_notice.manual_login_notice.read() };
        if let Some(notice) = notice {
            auth_for_manual_login_notice.manual_login_notice.set(None);
            let captcha_policy = manual_login_captcha_policy(notice);
            manual_captcha_input.set(captcha_policy.show_input);
            manual_captcha_required.set(captcha_policy.require_input);
            manual_login_info.set(Some(notice));
        }
    });

    // 检查服务端是否启用了 WeChat 验证码登录功能
    {
        let client = auth.client.clone();
        use_effect(move || {
            let client = client.clone();
            spawn(async move {
                if let Ok(resp) = client.wechat_enabled().await {
                    wechat_enabled.set(resp.enabled);
                }
            });
        });
    }

    // 每次切换登录/注册/验证码模式时清空表单与状态
    use_effect(move || {
        let _ = mode();
        name.set(String::new());
        email.set(String::new());
        password.set(String::new());
        password_confirm.set(String::new());
        captcha_code.set(String::new());
        error_msg.set(None);
        loading.set(false);
    });

    let info = (*manual_login_info.read()).map(|notice| manual_login_notice_text(t, notice));

    rsx! {
        document::Link {
            rel: "stylesheet",
            href: asset!("/assets/styling/login_landing.css"),
        }
        div { class: "ains-landing",
            // ── 左侧：登录/注册表单 ──
            div { class: "ains-landing__left",
                div { class: "ains-landing__brand",
                    h1 { class: "ains-landing__brand-title", "AINS" }
                    p { class: "ains-landing__brand-subtitle", {t.login_brand_subtitle} }
                }
                AuthForm {
                    mode,
                    name,
                    email,
                    password,
                    password_confirm,
                    captcha_code: Some(captcha_code),
                    remember: Some(remember),
                    loading: *loading.read(),
                    error: error_msg.read().clone(),
                    info,
                    show_captcha_input: *wechat_enabled.read() || *manual_captcha_input.read(),
                    on_forgot: move |_: MouseEvent| {
                        nav.push(Route::ForgotPassword {});
                    },
                    on_submit: move |payload: AuthPayload| {
                        if *loading.read() {
                            return;
                        }
                        // 前端表单校验
                        if payload.mode == AuthMode::Login
                            && (*wechat_enabled.read() || *manual_captcha_required.read())
                            && payload.captcha_code.trim().is_empty()
                        {
                            error_msg.set(Some(t.login_captcha_empty.to_string()));
                            return;
                        }
                        if payload.email.trim().is_empty() {
                            error_msg.set(Some(t.login_email_empty.to_string()));
                            return;
                        }
                        if payload.password.is_empty() {
                            error_msg.set(Some(t.login_password_empty.to_string()));
                            return;
                        }
                        if payload.mode == AuthMode::Register {
                            let name_trimmed = payload.name.trim();
                            if name_trimmed.is_empty() {
                                error_msg.set(Some(t.login_name_empty.to_string()));
                                return;
                            }
                            if name_trimmed.len() < 6 || name_trimmed.len() > 50 {
                                error_msg.set(Some(t.login_name_length.to_string()));
                                return;
                            }
                            if payload.password != payload.password_confirm {
                                error_msg.set(Some(t.login_password_mismatch.to_string()));
                                return;
                            }
                        }
                        let payload_email = payload.email.clone();
                        let payload_password = payload.password.clone();
                        let payload_name = payload.name.clone();
                        let payload_captcha_code = payload.captcha_code.clone();
                        let payload_mode = payload.mode;
                        let payload_remember = payload.remember;

                        let mut auth_async = auth.clone();
                        let bus_async = log_bus;
                        let nav_async = nav;

                        loading.set(true);
                        error_msg.set(None);
                        manual_login_info.set(None);

                        let mut mode_check = mode;

                        spawn(async move {
                            let result: Result<SubmitAction, client_api::ClientError> = match payload_mode {
                                AuthMode::Login => {
                                    let path = "/api/public/auth/login".to_string();
                                    let captcha = if payload_captcha_code.is_empty() {
                                        None
                                    } else {
                                        Some(payload_captcha_code.clone())
                                    };
                                    let res = auth_async
                                        .login(
                                            &payload_email,
                                            &payload_password,
                                            payload_remember,
                                            captcha,
                                        )
                                        .await;
                                    if *mode_check.read() == AuthMode::Login {
                                        push_log_result(bus_async, HttpMethod::Post, &path, &res);
                                    }
                                    res.map(|_| SubmitAction::Nothing)
                                }
                                AuthMode::Register => {
                                    let path = "/api/public/auth/register".to_string();
                                    let res = auth_async
                                        .register(
                                            &payload_email,
                                            &payload_password, // 导航由 auth.user 变化触发的 use_effect 统一处理
                                            &payload_name,
                                            payload_remember,
                                            &payload.password_confirm,
                                        )
                                        .await;
                                    if *mode_check.read() == AuthMode::Register {
                                        push_log_result(bus_async, HttpMethod::Post, &path, &res);
                                    }
                                    res.map(|outcome| match outcome {
                                        RegisterOutcome::LoggedIn => SubmitAction::Nothing,
                                        RegisterOutcome::NeedsManualLogin => SubmitAction::ReturnToLogin,
                                        RegisterOutcome::NeedsVerification { email } => {
                                            SubmitAction::NavigateToVerify {
                                                email,
                                            }
                                        }
                                    })
                                }
                            };
                            loading.set(false);
                            match result {
                                Ok(SubmitAction::Nothing) => {}
                                Ok(SubmitAction::ReturnToLogin) => {
                                    mode_check.set(AuthMode::Login);
                                }
                                Ok(SubmitAction::NavigateToVerify { email }) => {
                                    nav_async.push(Route::VerifyEmail { email });
                                }
                                Err(err) => {
                                    if *mode_check.read() == payload_mode {
                                        error_msg
                                            .set(
                                                Some(humanize_error(&err, ErrorContext::Auth, i18n.lang())),
                                            );
                                    }
                                }
                            }
                        });
                    },
                }
            }

            // ── 右侧：二维码 + 版权 + GitHub ──
            div { class: "ains-landing__right",
                div { class: "ains-landing__info-card",
                    // 公众号二维码区域
                    div { class: "ains-landing__qr-section",
                        div { class: "ains-landing__qr-placeholder",
                            img {
                                class: "ains-landing__qr-img",
                                src: QRCODE_IMG,
                                alt: "openpick qrcode",
                            }
                            p { class: "ains-landing__qr-label", {t.login_qr_label} }
                            p { class: "ains-landing__qr-hint", {t.login_qr_hint} }
                        }
                    }

                    // 分隔线
                    div { class: "ains-landing__divider" }

                    // GitHub 项目地址
                    div { class: "ains-landing__github",
                        a {
                            class: "ains-landing__github-link",
                            href: "https://github.com/aiqubits/ains",
                            target: "_blank",
                            rel: "noopener noreferrer",
                            svg {
                                width: "20",
                                height: "20",
                                view_box: "0 0 24 24",
                                fill: "currentColor",
                                path { d: "M12 0C5.37 0 0 5.37 0 12c0 5.31 3.435 9.795 8.205 11.385.6.105.825-.255.825-.57 0-.285-.015-1.23-.015-2.235-3.015.555-3.795-.735-4.035-1.41-.135-.345-.72-1.41-1.23-1.695-.42-.225-1.02-.78-.015-.795.945-.015 1.62.87 1.845 1.23 1.08 1.815 2.805 1.305 3.495.99.105-.78.42-1.305.765-1.605-2.67-.3-5.46-1.335-5.46-5.925 0-1.305.465-2.385 1.23-3.225-.12-.3-.54-1.53.12-3.18 0 0 1.005-.315 3.3 1.23.96-.27 1.98-.405 3-.405s2.04.135 3 .405c2.295-1.56 3.3-1.23 3.3-1.23.66 1.65.24 2.88.12 3.18.765.84 1.23 1.905 1.23 3.225 0 4.605-2.805 5.625-5.475 5.925.435.375.81 1.095.81 2.22 0 1.605-.015 2.895-.015 3.3 0 .315.225.69.825.57A12.02 12.02 0 0024 12c0-6.63-5.37-12-12-12z" }
                            }
                            span { {t.login_github_label} }
                        }
                    }

                    // 版权声明
                    div { class: "ains-landing__copyright",
                        p { class: "ains-landing__copyright-text", "© 2026 AINS. All rights reserved." }
                        p { class: "ains-landing__copyright-sub", {t.login_copyright_sub} }
                    }
                }
            }
        }

        // 语言切换浮动按钮
        LanguageSwitcher { variant: LanguageSwitcherVariant::Floating }
    }
}
