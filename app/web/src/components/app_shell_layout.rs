use dioxus::prelude::*;

use ui::{AppShell, I18nContext, NavKey, Sidebar, TopHeader};

/// 搜索信号包装类型，通过 `use_context_provider` 全局注入。
///
/// 在 `AppShellLayout` 中创建，`Users` 等消费方通过 `use_context` 读取。
#[derive(Clone, Copy)]
pub struct SearchSignal(pub Signal<String>);

use crate::Route;
use crate::auth::AuthState;
use crate::components::{ConfirmDialog, TokenExpiryGuard};

/// 将 `AppShell` + `Sidebar` + `TopHeader` + `Outlet<Route>` 装配在一起的 web 专用布局。
///
/// 持有移动端侧边栏抽屉状态 (`sidebar_open`)，并从 `AuthState` 读取当前用户身份
/// 注入到 `TopHeader`。
#[component]
pub fn AppShellLayout() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    let mut sidebar_open = use_signal(|| false);
    let search_value = use_signal(String::new);
    use_context_provider(|| SearchSignal(search_value));
    let nav = use_navigator();
    let auth = use_context::<AuthState>();

    // 登出确认弹窗状态
    let mut show_logout_confirm = use_signal(|| false);

    let route = use_route::<Route>();
    let active_nav = match route {
        Route::PersonalCenter {} => NavKey::PersonalCenter,
        Route::AgentChat {} => NavKey::AgentChat,
        Route::Skills {} => NavKey::Skills,
        Route::Memory {} => NavKey::Memory,
        Route::Tools {} => NavKey::Tools,
        Route::Dashboard {} => NavKey::Dashboard,
        Route::Users {} => NavKey::Users,
        Route::Tenants {} => NavKey::Tenants,
        Route::Channels {} => NavKey::Channels,
        Route::Metering {} => NavKey::Metering,
        Route::Plans {} => NavKey::Plans,
        Route::Orders {} => NavKey::Orders,
        // Settings 在本布局内但没有专用 NavKey，沿用个人中心高亮。
        Route::Settings {} => NavKey::PersonalCenter,
        // 通配臂——此布局仅包裹 PersonalCenter / Dashboard / Settings / Users / Tenants / Channels / Metering / Plans / Orders（见 main.rs 路由定义），
        // 其他 Route 变体不应到达本布局。若未来新增路由加入此布局，
        // 编译器不会警告，需手动在此处补充分支。
        _ => NavKey::PersonalCenter,
    };

    // 从 AuthState 派生展示用身份信息
    let (user_name, user_email) = match auth.user.read().as_ref() {
        Some(u) => (u.name.clone(), u.email.clone()),
        None => (
            t.app_shell_guest.to_string(),
            t.app_shell_not_logged_in.to_string(),
        ),
    };
    // 检查当前用户角色，控制 admin 模块的可见性
    let is_admin = auth
        .user
        .read()
        .as_ref()
        .map(|u| u.is_admin())
        .unwrap_or(false);

    rsx! {
        AppShell {
            sidebar: rsx! {
                Sidebar {
                    open: sidebar_open,
                    on_close: move |_| sidebar_open.set(false),
                    active: active_nav,
                    show_admin_modules: is_admin,
                    on_select: move |key| {
                        let target = match key {
                            NavKey::PersonalCenter => Route::PersonalCenter {},
                            NavKey::AgentChat => Route::AgentChat {},
                            NavKey::Skills => Route::Skills {},
                            NavKey::Memory => Route::Memory {},
                            NavKey::Tools => Route::Tools {},
                            NavKey::Dashboard => Route::Dashboard {},
                            NavKey::Users => Route::Users {},
                            NavKey::Tenants => Route::Tenants {},
                            NavKey::Channels => Route::Channels {},
                            NavKey::Metering => Route::Metering {},
                            NavKey::Plans => Route::Plans {},
                            NavKey::Orders => Route::Orders {},
                        };
                        sidebar_open.set(false);
                        nav.push(target);
                    },
                }
            },
            top_header: rsx! {
                TopHeader {
                    on_sidebar_toggle: move |_| sidebar_open.toggle(),
                    search_value,
                    user_name,
                    user_email,
                    on_settings_click: move |_| {
                        nav.push(Route::Settings {});
                    },
                    on_logout: move |_| {
                        show_logout_confirm.set(true);
                    },
                }
            },
            Outlet::<Route> {}
            TokenExpiryGuard {}

            // 登出确认弹窗
            ConfirmDialog {
                open: *show_logout_confirm.read(),
                title: t.app_shell_confirm_logout.to_string(),
                message: t.app_shell_confirm_logout_msg.to_string(),
                danger: true,
                on_confirm: move |_| {
                    let mut auth_async = auth.clone();
                    let nav_async = nav;
                    spawn(async move {
                        auth_async.logout_async().await;
                        nav_async.replace(Route::LoginLanding {});
                    });
                    show_logout_confirm.set(false);
                },
                on_cancel: move |_| show_logout_confirm.set(false),
            }
        }
    }
}
