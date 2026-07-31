use dioxus::prelude::*;

/// 轻量非阻塞提示的语义色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Success,
    Warning,
}

impl NoticeKind {
    fn class(self) -> &'static str {
        match self {
            Self::Info => "ains-notice--info",
            Self::Success => "ains-notice--success",
            Self::Warning => "ains-notice--warning",
        }
    }
}

/// 单条提示。`id` 用于自动消失去重（宿主每次推送递增 id）。
#[derive(Debug, Clone, PartialEq)]
pub struct NoticeItem {
    pub id: u64,
    pub text: String,
    pub kind: NoticeKind,
}

/// 轻量非阻塞提示（单条、自动消失、不拦截交互）。
///
/// 宿主持有 `Signal<Option<NoticeItem>>` 推送提示；组件在 `auto_dismiss_ms`
/// 后回调 `on_dismiss(id)`，宿主据此清空（仅当仍是同一 id）。固定定位、
/// `pointer-events: none`，不阻塞页面其余操作。
#[component]
pub fn NoticeToast(
    notice: ReadSignal<Option<NoticeItem>>,
    on_dismiss: EventHandler<u64>,
    #[props(default = 2600)] auto_dismiss_ms: u64,
) -> Element {
    // 计时自动消失：读取 notice 使 use_resource 对其变化敏感（新提示到来
    // 会取消上一个计时器，只保留最新一条的销毁任务）。
    use_resource(move || {
        let current_id = notice.read().as_ref().map(|n| n.id);
        async move {
            let Some(id) = current_id else {
                return;
            };
            #[cfg(target_arch = "wasm32")]
            gloo_timers::future::TimeoutFuture::new(auto_dismiss_ms.min(u32::MAX as u64) as u32)
                .await;
            #[cfg(not(target_arch = "wasm32"))]
            tokio::time::sleep(std::time::Duration::from_millis(auto_dismiss_ms)).await;
            on_dismiss.call(id);
        }
    });

    let Some(item) = notice.read().clone() else {
        return rsx! {};
    };

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/notice_toast.css") }
        div {
            class: format!("ains-notice {}", item.kind.class()),
            role: "status",
            aria_live: "polite",
            span { class: "ains-notice__text", "{item.text}" }
        }
    }
}
