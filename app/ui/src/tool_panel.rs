use dioxus::prelude::*;

use crate::{EN, I18nContext};

/// 工具分类（视图模型，宿主从 agent-core `ToolCategory` 映射；
/// `Meta` 对应 AgentInternal——权限/交互类元工具，`Mcp` 为远程桥接）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategoryView {
    Compute,
    FileSystem,
    System,
    Network,
    Browser,
    Meta,
    Mcp,
}

impl ToolCategoryView {
    fn class(self) -> &'static str {
        match self {
            Self::Compute => "ains-tools__badge--compute",
            Self::FileSystem => "ains-tools__badge--fs",
            Self::System => "ains-tools__badge--system",
            Self::Network => "ains-tools__badge--network",
            Self::Browser => "ains-tools__badge--browser",
            Self::Meta => "ains-tools__badge--meta",
            Self::Mcp => "ains-tools__badge--mcp",
        }
    }
}

/// 工具卡片视图模型（宿主从 `ToolRuntime::api_schemas()` + `category()` 映射）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCardView {
    pub name: String,
    pub description: String,
    pub category: ToolCategoryView,
}

/// Tool 执行面板（Phase 6.7）：列出当前运行时可用工具（名称 + 描述 +
/// 分类徽标；元工具高亮区分于普通能力工具）。
#[component]
pub fn ToolPanel(tools: ReadSignal<Vec<ToolCardView>>) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    let category_label = |category: ToolCategoryView| match category {
        ToolCategoryView::Compute => t.tool_cat_compute,
        ToolCategoryView::FileSystem => t.tool_cat_filesystem,
        ToolCategoryView::System => t.tool_cat_system,
        ToolCategoryView::Network => t.tool_cat_network,
        ToolCategoryView::Browser => t.tool_cat_browser,
        ToolCategoryView::Meta => t.tool_cat_meta,
        ToolCategoryView::Mcp => t.tool_cat_mcp,
    };

    let list = tools.read().clone();
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/tool_panel.css") }
        section { class: "ains-tools",
            header { class: "ains-tools__header",
                h2 { class: "ains-tools__title", {t.tool_panel_title} }
                p { class: "ains-tools__subtitle", {t.tool_panel_subtitle} }
            }
            if list.is_empty() {
                p { class: "ains-tools__empty", {t.tool_panel_empty} }
            }
            div { class: "ains-tools__grid",
                for tool in list {
                    div { key: "{tool.name}", class: "ains-tools__card",
                        div { class: "ains-tools__card-head",
                            code { class: "ains-tools__card-name", "{tool.name}" }
                            span { class: format!("ains-tools__badge {}", tool.category.class()),
                                {category_label(tool.category)}
                            }
                        }
                        p { class: "ains-tools__card-desc", "{tool.description}" }
                    }
                }
            }
        }
    }
}
