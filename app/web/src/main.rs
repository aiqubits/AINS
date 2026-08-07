use dioxus::prelude::*;

use auth::AuthState;
use components::{AppShellLayout, LogBus, RequireAuth};
use i18n::Language;
use ui::I18nContext;
use views::{
    AgentChat, Auth, Channels, Dashboard, ForgotPassword, LoginLanding, Memory, Metering, NotFound,
    Orders, PersonalCenter, Plans, ResetPassword, Settings, Skills, Tenants, Tools, Users,
    VerifyEmail,
};

mod agent;
mod api;
mod auth;
mod balance;
mod components;
mod views;

/// 测试共享设施（仅测试构建）：进程级全局状态（ui crate 的
/// PERSIST_ERROR / TOOL_STATE_LOAD_ERROR 信号与 PERSIST_STATE 状态机）
/// 跨测试存活且被多个测试模块（views::tools / agent::service）读写，
/// cargo test 并行执行时交叉污染会导致偶发断言失败——凡读写这些全局
/// 状态的测试必须持有 [`SIGNAL_TEST_LOCK`] 串行执行。
#[cfg(test)]
mod test_shared {
    pub static SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
enum Route {
    // ─ 公开路由（无需认证）──
    #[route("/")]
    LoginLanding {},
    #[route("/auth")]
    Auth {},
    #[route("/forgot-password")]
    ForgotPassword {},
    #[route("/reset-password")]
    #[route("/reset-password/:email")]
    ResetPassword { email: Option<String> },
    #[route("/verify-email/:email")]
    VerifyEmail { email: String },

    // ── 受保护路由（需登录）──
    #[layout(RequireAuth)]
        #[layout(AppShellLayout)]
            #[route("/personal")]
            PersonalCenter {},
            #[route("/agent")]
            AgentChat {},
            #[route("/skills")]
            Skills {},
            #[route("/memory")]
            Memory {},
            #[route("/tools")]
            Tools {},
            #[route("/settings")]
            Settings {},
            #[layout(crate::components::RequireAdmin)]
                #[route("/dashboard")]
                Dashboard {},
                #[route("/users")]
                Users {},
                #[route("/tenants")]
                Tenants {},
                #[route("/channels")]
                Channels {},
                #[route("/metering")]
                Metering {},
                #[route("/plans")]
                Plans {},
                #[route("/orders")]
                Orders {},
            #[end_layout]
        #[end_layout]
    #[end_layout]

    // ── 404 兜底 ──
    #[route("/:..route")]
    NotFound { route: Vec<String> },
}

const FAVICON: Asset = asset!("/assets/favicon.ico");
const MAIN_CSS: Asset = asset!("/assets/main.css");

fn main() {
    dioxus::launch(App);
}

fn init_language() -> Language {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            // 1) 优先读取 localStorage 中的用户选择
            if let Some(val) = window
                .local_storage()
                .ok()
                .and_then(|s| s?.get_item("ains_lang_v1").ok().flatten())
            {
                match val.as_str() {
                    "zh" => return Language::Zh,
                    "en" => return Language::En,
                    _ => {}
                }
            }
            // 2) 未存储过 → 探测浏览器语言
            if let Some(lang) = window.navigator().language() {
                // TODO: distinguish zh-Hant from zh-Hans when adding Traditional Chinese
                if !lang.is_empty() && lang.starts_with("zh") {
                    return Language::Zh;
                }
            }
            // Safari 隐私模式回退：navigator.languages
            let languages = window.navigator().languages();
            if languages.length() > 0 {
                let first = languages.get(0);
                if let Some(lang_str) = first.as_string()
                    && lang_str.starts_with("zh")
                {
                    return Language::Zh;
                }
            }
        }
    }
    // wasm32（无存储 + 非中文浏览器）和非 wasm32（desktop/mobile）统一返回 En
    Language::En
}

#[component]
fn App() -> Element {
    let lang = init_language();
    use_context_provider(|| I18nContext::new(lang));
    use_context_provider(AuthState::new);
    // LogBus 仍需提供给 Dashboard 的服务链路追踪控制台使用（右上角 Toast 提示已移除）。
    use_context_provider(LogBus::new);
    let auth = use_context::<AuthState>();
    // Agent 视图（/agent）直接消费 Client 上下文（token 跨 clone 共享），
    // 使 agent_chat 视图可被 desktop 端复用。
    use_context_provider(|| auth.client.clone());

    // 应用启动时一次性恢复 localStorage 中的会话并拉取真实用户资料。
    // 必须放在路由挂载之前——这样无论首屏路由是 /、/users、/settings 还是 /auth，
    // 都已经有正确的 auth.user 状态，避免 "记住登录" 后直接刷新受保护页面
    // 表现为未登录的 BUG（Issue B1）。
    let mut once_flag = use_signal(|| false);
    use_effect(move || {
        if !*once_flag.read() {
            once_flag.set(true);
            // 闭包需要 FnMut (可能被多次调用) 而 spawn 需 FnOnce (会移动 auth)；
            // 在闭包内 clone 后再 move 进 spawn，避免与 use_effect 的 FnMut 冲突。
            let mut auth_clone = auth.clone();
            spawn(async move {
                auth_clone.restore_from_storage_async().await;
            });
        }
    });

    rsx! {
        document::Link { rel: "icon", href: FAVICON }
        ui::GlobalStyles {}
        document::Link { rel: "stylesheet", href: MAIN_CSS }

        Router::<Route> {}
    }
}
