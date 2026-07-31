//! Tool 执行面板视图（Phase 6.7）：展示当前运行时已注册的工具。

use dioxus::prelude::*;

use agent_core::tools::ToolCategory;
use ui::{I18nContext, ToolCardView, ToolCategoryView, ToolPanel};

use crate::agent::service;

/// agent-core 工具分类 → 面板徽标视图（MCP 桥接工具按名称前缀识别，
/// 其余按 `Tool::category()` 自报分类）。
fn to_category_view(name: &str, category: ToolCategory) -> ToolCategoryView {
    if name.starts_with("mcp__") {
        return ToolCategoryView::Mcp;
    }
    match category {
        ToolCategory::Compute => ToolCategoryView::Compute,
        ToolCategory::FileSystem => ToolCategoryView::FileSystem,
        ToolCategory::System => ToolCategoryView::System,
        ToolCategory::Network => ToolCategoryView::Network,
        ToolCategory::Browser => ToolCategoryView::Browser,
        ToolCategory::AgentInternal => ToolCategoryView::Meta,
    }
}

#[component]
pub fn Tools() -> Element {
    let _i18n = use_context::<I18nContext>();
    // 轻量同步快照：仅构造 ToolRuntime 读 schema + 分类，不装配 Kernel/会话。
    let tools = use_signal(|| {
        service::tool_schema_snapshot()
            .into_iter()
            .map(|(name, description, category)| ToolCardView {
                category: to_category_view(&name, category),
                name,
                description,
            })
            .collect::<Vec<_>>()
    });

    rsx! {
        div { style: "padding:16px;",
            ToolPanel { tools }
        }
    }
}
