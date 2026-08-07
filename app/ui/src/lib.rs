//! AINS 共享 UI 组件库。
//!
//! 本 crate 提供两类组件：
//! - **展示组件**：`button` / `badge` / `text_input` / `app_shell` / `sidebar` / `top_header` 等。
//! - **旧组件**（保留给 desktop / mobile 使用）：`Hero`、`Navbar`。
//!
//! 设计令牌（颜色 / 字体 / 玻璃面板等）以 CSS 自定义属性形式集中在
//! `assets/styling/tokens.css`，由各组件的 stylesheet 引用。

// 旧组件 —— 保留以兼容 desktop / mobile 构建。
mod hero;
pub use hero::Hero;

mod navbar;
pub use navbar::Navbar;

// 展示类原语
mod button;
pub use button::{Button, ButtonType};

mod badge;
pub use badge::{Badge, BadgeVariant};

mod text_input;
pub use text_input::{InputType, TextInput};

// 布局组件
mod app_shell;
pub use app_shell::AppShell;

mod sidebar;
pub use sidebar::{NavKey, Sidebar};

mod top_header;
pub use top_header::TopHeader;

mod toast;
pub use toast::{ToastEntry, ToastKind, ToastMethod, ToastStack};

mod modal;
pub use modal::Modal;

mod auth_form;
pub use auth_form::{AuthForm, AuthMode, AuthPayload};

mod data_table;
pub use data_table::{Align, Column, DataTable};

mod stats_card;
pub use stats_card::{StatsAccent, StatsCard, StatsValueColor};

mod route_card;
pub use route_card::{RouteCard, RouteMethod};

mod code_console;
pub use code_console::{CodeConsole, ConsoleKind, ConsoleLine};

mod global_styles;
pub use global_styles::GlobalStyles;

// Agent UI（Phase 6）—— 纯展示组件，视图模型由宿主从 agent-core 映射。
mod chat_view;
pub use chat_view::{
    ChatInput, ChatItem, ChatRole, ChatView, ChatViewState, SlashCommandView, ToolCallCard,
    ToolCallStatus,
};

mod permission_dialog;
pub use permission_dialog::{PermissionChoice, PermissionDialog, PermissionRequestView};

mod permission_controls;
pub use permission_controls::{PermissionModeSwitcher, PermissionModeView, PlanModeIndicator};

mod skills_panel;
pub use skills_panel::{SkillCard, SkillDetailView, SkillTrustView, SkillsPanel};

mod notice_toast;
pub use notice_toast::{NoticeItem, NoticeKind, NoticeToast};

mod agent_status;
pub use agent_status::{AgentStatus, AgentStatusView};

mod tool_panel;
pub use tool_panel::{
    PERSIST_ERROR, PERSIST_IDLE, PERSIST_PENDING, PERSIST_RUNNING, PERSIST_STATE, TOOL_PANEL_CSS,
    TOOL_STATE_LOAD_ERROR, ToolCardView, ToolCategoryView, ToolPanel, ToolStateBanner,
    ToolStateBannerKind, persist_on_round_done, persist_on_toggle, persist_task_in_flight,
    should_sync_persist_error, sync_persist_error,
};

mod memory_viewer;
pub use memory_viewer::{MemoryCard, MemoryViewer};

mod todo_list;
pub use todo_list::{TodoItemView, TodoList, parse_todo_markdown};

// i18n
pub use i18n::{EN, I18nContext, Language, Translations, ZH, tf};

mod language_switcher;
pub use language_switcher::{LanguageSwitcher, LanguageSwitcherVariant};
