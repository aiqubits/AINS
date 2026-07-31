use dioxus::prelude::*;

use client_api::{Client, ClientConfig};
use i18n::Language;
use ui::{I18nContext, Navbar};
use views::{AgentChat, Blog, Home, Memory, Skills, Tools};

mod agent;
mod views;

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
enum Route {
    #[layout(DesktopNavbar)]
    #[route("/")]
    Home {},
    #[route("/blog/:id")]
    Blog { id: i32 },
    #[route("/agent")]
    AgentChat {},
    #[route("/skills")]
    Skills {},
    #[route("/memory")]
    Memory {},
    #[route("/tools")]
    Tools {},
}

const MAIN_CSS: Asset = asset!("/assets/main.css");

fn main() {
    dioxus::launch(App);
}

fn init_language() -> Language {
    // desktop: no persistence, default to En
    Language::En
}

/// Native 端 Client：读取 `AINS_API_URL`（未设置回退本地服务端），
/// 建连层重试关闭（GatewayModelClient 自带流式重试，避免双层叠加）。
/// 用户提供的 URL 非法时告警并回退默认值，不使桌面应用启动崩溃。
///
/// 桌面端暂无登录流：网关认证经 `AINS_API_TOKEN` 环境变量注入；
/// 未设置时告警提示（模型网关将返回 401/403，前端归类为可恢复错误）。
fn make_client() -> Client {
    const DEFAULT_API_URL: &str = "http://127.0.0.1:8080";
    let base = std::env::var("AINS_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let client =
        Client::new(ClientConfig::new(base.clone()).with_max_retries(0)).unwrap_or_else(|err| {
            // desktop 未安装 tracing subscriber，用 stderr 保证误配可诊断
            eprintln!("invalid AINS_API_URL `{base}` ({err}); falling back to {DEFAULT_API_URL}");
            Client::new(ClientConfig::new(DEFAULT_API_URL).with_max_retries(0))
                .expect("default client config must be valid")
        });
    match std::env::var("AINS_API_TOKEN") {
        Ok(token) if !token.trim().is_empty() => client.set_token(token),
        _ => eprintln!(
            "AINS_API_TOKEN not set; gateway calls from the agent view will be unauthenticated"
        ),
    }
    client
}

#[component]
fn App() -> Element {
    let lang = init_language();
    use_context_provider(|| I18nContext::new(lang));
    // Agent 视图消费 Client 上下文（与 web 端同构，token 跨 clone 共享）。
    use_context_provider(make_client);

    rsx! {
        ui::GlobalStyles {}
        document::Link { rel: "stylesheet", href: MAIN_CSS }

        Router::<Route> {}
    }
}

/// A desktop-specific Router around the shared `Navbar` component
/// which allows us to use the desktop-specific `Route` enum.
#[component]
fn DesktopNavbar() -> Element {
    rsx! {
        Navbar {
            Link { to: Route::Home {}, "Home" }
            Link { to: Route::Blog { id: 1 }, "Blog" }
            Link { to: Route::AgentChat {}, "Agent" }
            Link { to: Route::Skills {}, "Skills" }
            Link { to: Route::Memory {}, "Memory" }
            Link { to: Route::Tools {}, "Tools" }
        }

        Outlet::<Route> {}
    }
}
