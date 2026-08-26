//! Channel 管理视图（admin/system）。
//!
//! 参照 `users.rs` 的架构模式，完整接入 `client-api` 的 channels 管理 API。

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use client_api::{ChannelResponse, TenantResponse};
use dioxus::prelude::dioxus_router::Navigator;
use dioxus::prelude::*;
use dioxus_icons::lucide::{
    ChevronDown, LoaderCircle, Pencil, Plus, ShieldHalf, Trash2, TriangleAlert,
};

use ui::{
    Align, Badge, BadgeVariant, Button, ButtonType, Column, DataTable, I18nContext, InputType,
    Modal, TextInput, Translations, tf,
};

use crate::api::{ErrorContext, humanize_error};
use crate::auth::AuthState;
use crate::components::{
    ConfirmDialog, HttpMethod, LogBus, SearchSignal, push_log_err, push_log_ok,
};

/// DOM id of the tenant dropdown scroll panel — used by the infinite-scroll
/// handler to read the panel's scroll position via `element_near_bottom`.
const TENANT_PANEL_ID: &str = "channel-tenant-dropdown-panel";

const ALL_CAPABILITIES: &[(&str, &str)] = &[
    ("chat", "Chat"),
    ("vision", "Vision"),
    ("stt", "STT"),
    ("tts", "TTS"),
    ("websearch", "WebSearch"),
    ("embedding", "Embedding"),
];

#[derive(Debug, Clone)]
enum ListState {
    Loading,
    Loaded(Vec<ChannelResponse>),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalKind {
    None,
    Create,
    Edit,
    DeleteConfirm,
}

#[derive(Clone, Copy)]
struct ChannelsSignals {
    modal_kind: Signal<ModalKind>,
    editing_channel: Signal<Option<ChannelResponse>>,
    deleting_channel: Signal<Option<ChannelResponse>>,
    form_name: Signal<String>,
    form_protocol: Signal<String>,
    form_base_url: Signal<String>,
    form_api_key: Signal<String>,
    form_models: Signal<String>,
    form_capabilities: Signal<Vec<String>>,
    form_weight: Signal<String>,
    form_is_active: Signal<bool>,
    form_tenant_id: Signal<String>,
    /// 自定义租户下拉是否展开（原生 select 的弹层无法与 Modal 风格对齐，故自绘）。
    tenant_dropdown_open: Signal<bool>,
    /// 租户下拉「无限滚动」分页状态：下一页页码、是否还有更多、是否正在加载。
    tenant_next_page: Signal<u64>,
    tenant_has_more: Signal<bool>,
    tenant_loading: Signal<bool>,
    submitting: Signal<bool>,
    form_error: Signal<Option<String>>,
    list_version: Signal<u64>,
}

/// 分页状态（快照值 + 信号），用于 `render_table` 传参。
#[derive(Clone, Copy)]
struct PaginationState {
    page: u64,
    per_page: u64,
    total: u64,
    total_pages: u64,
    page_signal: Signal<u64>,
    per_page_signal: Signal<u64>,
}

#[component]
pub fn Channels() -> Element {
    let auth = use_context::<AuthState>();
    let log_bus = use_context::<LogBus>();
    let nav = use_navigator();
    let locale = use_context::<I18nContext>();
    let t = locale.t();

    let list = use_signal(|| ListState::Loading);
    let list_version = use_signal(|| 0u64);
    let page = use_signal(|| 1u64);
    let per_page = use_signal(|| 20u64);
    let total = use_signal(|| 0u64);
    let total_pages = use_signal(|| 0u64);
    let SearchSignal(search_query) = use_context::<SearchSignal>();
    let available_tenants = use_signal(Vec::<TenantResponse>::new);

    let signals = ChannelsSignals {
        modal_kind: use_signal(|| ModalKind::None),
        editing_channel: use_signal(|| Option::<ChannelResponse>::None),
        deleting_channel: use_signal(|| Option::<ChannelResponse>::None),
        form_name: use_signal(String::new),
        form_protocol: use_signal(|| "openai".to_string()),
        form_base_url: use_signal(String::new),
        form_api_key: use_signal(String::new),
        form_models: use_signal(String::new),
        form_capabilities: use_signal(Vec::new),
        form_weight: use_signal(|| "1".to_string()),
        form_is_active: use_signal(|| true),
        form_tenant_id: use_signal(String::new),
        tenant_dropdown_open: use_signal(|| false),
        tenant_next_page: use_signal(|| 2u64),
        tenant_has_more: use_signal(|| false),
        tenant_loading: use_signal(|| false),
        submitting: use_signal(|| false),
        form_error: use_signal(|| Option::<String>::None),
        list_version,
    };

    // 搜索变更时自动重置到第 1 页
    {
        let search_for_reset = search_query;
        let mut page_for_reset = page;
        let first_run: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(true))).clone();
        use_effect(move || {
            let _ = search_for_reset.read();
            if first_run.get() {
                first_run.set(false);
            } else {
                page_for_reset.set(1);
            }
        });
    }

    // 租户列表拉取：独立于分页 effect，挂载后仅执行一次。
    // 用途：创建/编辑时的租户下拉选择 + 列表「所属租户」列的 ID→名称解析。
    // 之前该请求耦合在分页 effect 中，位于 list_channels 的早退守卫之后，一旦
    // 守卫命中或请求失败（错误被静默吞掉）就会导致 available_tenants 为空，
    // 使所属租户列回退显示原始租户 ID（默认租户因 ID 恰为可读字符串 "default"
    // 而看似正常，其余租户则暴露出 UUID）。
    {
        let client = auth.client.clone();
        let bus = log_bus;
        let auth_for_tenants = auth.clone();
        let mut at_inner = available_tenants;
        let mut has_more = signals.tenant_has_more;
        let mut next_page = signals.tenant_next_page;
        use_effect(move || {
            let client = client.clone();
            let auth_inner = auth_for_tenants.clone();
            spawn(async move {
                match client.list_tenants(1, 100).await {
                    Ok(data) => {
                        has_more.set(data.page < data.total_pages);
                        next_page.set(2);
                        at_inner.set(data.items);
                    }
                    Err(err) => {
                        if crate::api::handle_unauth(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/tenants", &err);
                    }
                }
            });
        });
    }

    // 数据拉取
    {
        let client = auth.client.clone();
        let bus = log_bus;
        let auth_for_effect = auth.clone();
        use_effect(move || {
            let _ = list_version();
            let current_page = page();
            let current_per_page = per_page();
            let client = client.clone();
            let mut list = list;
            let bus = bus;
            let auth_inner = auth_for_effect.clone();
            let version_check = list_version;
            let mut total_signal = total;
            let mut total_pages_signal = total_pages;
            let effect_lang = locale.lang();
            spawn(async move {
                let version = version_check();
                list.set(ListState::Loading);
                let lang = effect_lang;
                let res = client.list_channels(current_page, current_per_page).await;
                if version_check() != version
                    || page() != current_page
                    || per_page() != current_per_page
                {
                    return;
                }
                match res {
                    Ok(page_data) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/channels");
                        total_signal.set(page_data.total);
                        total_pages_signal.set(page_data.total_pages);
                        list.set(ListState::Loaded(page_data.items));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/channels", &err);
                        list.set(ListState::Error(humanize_error(
                            &err,
                            ErrorContext::ChannelManagement,
                            lang,
                        )));
                    }
                }
            });
        });
    }

    let list_snapshot = list.cloned();
    let search_text = search_query.cloned();
    let kind_snapshot = *signals.modal_kind.read();
    let form_error_snapshot = signals.form_error.read().clone();
    let submitting_snapshot = *signals.submitting.read();
    let editing_snapshot = signals.editing_channel.read().clone();
    let deleting_snapshot = signals.deleting_channel.read().clone();

    let current_user = auth.user.read().as_ref().cloned();
    let actor_is_system = current_user
        .as_ref()
        .map(|u| u.is_system())
        .unwrap_or(false);
    let actor_tenant_id = current_user
        .map(|u| u.tenant_id.clone())
        .unwrap_or_default();

    let page_val = page();
    let per_page_val = per_page();
    let total_val = total();
    let total_pages_val = total_pages();

    let mut signals_for_open = signals;
    let default_tenant_id = actor_tenant_id.clone();
    let tenant_name_map: HashMap<String, String> = available_tenants
        .cloned()
        .into_iter()
        .map(|t| (t.id, t.name))
        .collect();
    let open_create = move |_: MouseEvent| {
        signals_for_open.form_name.set(String::new());
        signals_for_open.form_protocol.set("openai".to_string());
        signals_for_open.form_base_url.set(String::new());
        signals_for_open.form_api_key.set(String::new());
        signals_for_open.form_models.set(String::new());
        signals_for_open.form_capabilities.set(Vec::new());
        signals_for_open.form_weight.set("1".to_string());
        signals_for_open.form_is_active.set(true);
        signals_for_open
            .form_tenant_id
            .set(default_tenant_id.clone());
        signals_for_open.form_error.set(None);
        signals_for_open.editing_channel.set(None);
        signals_for_open.modal_kind.set(ModalKind::Create);
    };

    rsx! {
        div { class: "ains-users",
            header { class: "ains-users__header",
                div { class: "ains-users__title-block",
                    h1 { class: "ains-users__title", "{t.channels_title}" }
                    p { class: "ains-users__subtitle", "{t.channels_subtitle}" }
                }
                div { class: "ains-users__header-actions",
                    span { class: "ains-users__guard-pill",
                        ShieldHalf {}
                        "{t.channels_guard_pill}"
                    }
                    Button { onclick: open_create,
                        Plus {}
                        "{t.channels_create_btn}"
                    }
                }
            }

            {
                render_table(
                    t,
                    list_snapshot,
                    search_text,
                    signals,
                    actor_is_system,
                    actor_tenant_id.clone(),
                    PaginationState {
                        page: page_val,
                        per_page: per_page_val,
                        total: total_val,
                        total_pages: total_pages_val,
                        page_signal: page,
                        per_page_signal: per_page,
                    },
                    tenant_name_map.clone(),
                )
            }

            {
                render_modal(
                    t,
                    kind_snapshot,
                    form_error_snapshot,
                    submitting_snapshot,
                    editing_snapshot,
                    deleting_snapshot,
                    signals,
                    auth.client.clone(),
                    log_bus,
                    auth.clone(),
                    nav,
                    actor_is_system,
                    actor_tenant_id,
                    available_tenants,
                )
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_table(
    t: &'static Translations,
    list_snapshot: ListState,
    search_text: String,
    signals: ChannelsSignals,
    actor_is_system: bool,
    actor_tenant_id: String,
    pagination: PaginationState,
    tenant_name_map: HashMap<String, String>,
) -> Element {
    match list_snapshot {
        ListState::Loading => rsx! {
            div { class: "ains-users__status",
                LoaderCircle { class: "ains-btn__spinner" }
                "{t.channels_loading}"
            }
        },
        ListState::Error(msg) => rsx! {
            div { class: "ains-users__status ains-users__status--error",
                TriangleAlert {}
                "{msg}"
            }
        },
        ListState::Loaded(items) => {
            let filtered: Vec<ChannelResponse> = if search_text.is_empty() {
                items
            } else {
                let q = search_text.to_lowercase();
                items
                    .into_iter()
                    .filter(|c| {
                        c.name.to_lowercase().contains(&q)
                            || c.base_url.to_lowercase().contains(&q)
                            || c.id.to_lowercase().contains(&q)
                    })
                    .collect()
            };
            let columns = build_columns(t);
            let rows: Vec<Element> = filtered
                .into_iter()
                .map(|ch| {
                    row_element(
                        t,
                        ch,
                        signals,
                        actor_is_system,
                        actor_tenant_id.clone(),
                        &tenant_name_map,
                    )
                })
                .collect();

            let PaginationState {
                page,
                per_page,
                total,
                total_pages,
                mut page_signal,
                mut per_page_signal,
            } = pagination;
            let has_prev = page > 1;
            let has_next = page < total_pages;

            let mut prev_sig = page_signal;
            let on_prev = move |_: MouseEvent| {
                prev_sig.set(page.saturating_sub(1).max(1));
            };
            let mut next_sig = page_signal;
            let on_next = move |_: MouseEvent| {
                next_sig.set((page + 1).min(total_pages));
            };
            let pagination_info = if total_pages == 0 {
                tf(t.users_count_simple, &[("total", &total.to_string())])
            } else {
                tf(
                    t.users_count_info,
                    &[
                        ("total", &total.to_string()),
                        ("page", &page.to_string()),
                        ("total_pages", &total_pages.to_string()),
                    ],
                )
            };

            rsx! {
                div { class: "ains-users__table-wrapper",
                    DataTable {
                        columns,
                        rows,
                        empty: Some(rsx! { "{t.channels_empty}" }),
                    }
                    div { class: "ains-pagination",
                        div { class: "ains-pagination__info", "{pagination_info}" }
                        div { class: "ains-pagination__controls",
                            button {
                                class: "ains-pagination__btn",
                                disabled: !has_prev,
                                onclick: on_prev,
                                "{t.users_prev_page}"
                            }
                            button {
                                class: "ains-pagination__btn",
                                disabled: !has_next,
                                onclick: on_next,
                                "{t.users_next_page}"
                            }
                            div { class: "ains-pagination__per-page",
                                span { "{t.users_per_page_label}" }
                                select {
                                    class: "ains-pagination__select",
                                    value: "{per_page}",
                                    onchange: move |evt| {
                                        if let Ok(v) = evt.value().parse::<u64>() {
                                            let v = v.clamp(1, 100);
                                            per_page_signal.set(v);
                                            page_signal.set(1);
                                        }
                                    },
                                    option { value: "20", "20" }
                                    option { value: "50", "50" }
                                    option { value: "100", "100" }
                                }
                                span { "{t.users_per_page_unit}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn capability_badges(capabilities: &serde_json::Value) -> String {
    if let Some(arr) = capabilities.as_array() {
        arr.iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        String::new()
    }
}

fn models_text(models: &serde_json::Value) -> String {
    if let Some(arr) = models.as_array() {
        arr.iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        String::new()
    }
}

fn row_element(
    t: &'static Translations,
    ch: ChannelResponse,
    signals: ChannelsSignals,
    actor_is_system: bool,
    actor_tenant_id: String,
    tenant_name_map: &HashMap<String, String>,
) -> Element {
    let id = ch.id.clone();
    let name = ch.name.clone();
    let protocol = ch.protocol_type.clone();
    let base_url = ch.base_url.clone();
    let is_active = ch.is_active;
    let weight = ch.weight;
    let capabilities_str = capability_badges(&ch.capabilities);
    let created = ch.created_at;
    let tenant_name = ch
        .tenant_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| tenant_name_map.get(&ch.tenant_id).map(|s| s.as_str()))
        .unwrap_or(&ch.tenant_id);

    // RBAC: system 可操作所有渠道；admin 只能操作自己租户内的渠道
    let is_same_tenant = ch.tenant_id == actor_tenant_id;
    let can_edit = actor_is_system || (!actor_tenant_id.is_empty() && is_same_tenant);
    let can_delete = actor_is_system || (!actor_tenant_id.is_empty() && is_same_tenant);

    let ch_for_edit = ch.clone();
    let ch_for_delete = ch.clone();
    let mut s_edit = signals;
    let mut s_delete = signals;

    let edit_handler = move |_: MouseEvent| {
        // Parse models and capabilities from JSON
        let models_str = models_text(&ch_for_edit.models);
        s_edit.form_name.set(ch_for_edit.name.clone());
        s_edit.form_protocol.set(ch_for_edit.protocol_type.clone());
        s_edit.form_base_url.set(ch_for_edit.base_url.clone());
        s_edit.form_api_key.set(String::new()); // Don't populate API key
        s_edit.form_models.set(models_str);
        s_edit
            .form_capabilities
            .set(parse_capability_vec(&ch_for_edit.capabilities));
        s_edit.form_weight.set(ch_for_edit.weight.to_string());
        s_edit.form_is_active.set(ch_for_edit.is_active);
        s_edit.form_tenant_id.set(ch_for_edit.tenant_id.clone());
        s_edit.form_error.set(None);
        s_edit.editing_channel.set(Some(ch_for_edit.clone()));
        s_edit.modal_kind.set(ModalKind::Edit);
    };
    let delete_handler = move |_: MouseEvent| {
        s_delete.form_error.set(None);
        s_delete.deleting_channel.set(Some(ch_for_delete.clone()));
        s_delete.modal_kind.set(ModalKind::DeleteConfirm);
    };

    rsx! {
        tr { key: "{id}",
            td {
                span {
                    class: "ains-table__name-cell",
                    title: "ID: {id}",
                    "data-id": "{id}",
                    "{name}"
                }
            }
            td {
                span {
                    class: "ains-table__name-cell",
                    title: "ID: {ch.tenant_id}",
                    "data-id": "{ch.tenant_id}",
                    "{tenant_name}"
                }
            }
            td {
                if protocol == "anthropic" {
                    Badge { variant: BadgeVariant::Admin, "Anthropic" }
                } else {
                    Badge { variant: BadgeVariant::User, "OpenAI" }
                }
            }
            td { class: "ains-table__mono ains-table__truncate",
                span { title: "{base_url}", "{base_url}" }
            }
            td {
                if is_active {
                    Badge { variant: BadgeVariant::User, "{t.channels_badge_active}" }
                } else {
                    Badge { variant: BadgeVariant::Admin, "{t.channels_badge_inactive}" }
                }
            }
            td { class: "ains-table__mono ains-table__align--right", "{weight}" }
            td { class: "ains-table__mono ains-table__truncate",
                span { title: "{capabilities_str}", "{capabilities_str}" }
            }
            td { class: "ains-table__mono", "{format_dt(&created)}" }
            td {
                if !can_edit && !can_delete {
                    div { class: "ains-table__row-actions",
                        span { class: "ains-table__protected",
                            ShieldHalf {}
                            "{t.channels_no_permission}"
                        }
                    }
                } else {
                    div { class: "ains-table__row-actions",
                        if can_edit {
                            button {
                                class: "ains-table__action",
                                title: "{t.channels_edit_title}",
                                onclick: edit_handler,
                                Pencil {}
                            }
                        }
                        if can_delete {
                            button {
                                class: "ains-table__action ains-table__action--danger",
                                title: "{t.channels_delete_title}",
                                onclick: delete_handler,
                                Trash2 {}
                            }
                        }
                    }
                }
            }
        }
    }
}

fn parse_capability_vec(val: &serde_json::Value) -> Vec<String> {
    if let Some(arr) = val.as_array() {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    } else {
        Vec::new()
    }
}

fn close_all(mut signals: ChannelsSignals) {
    signals.modal_kind.set(ModalKind::None);
    signals.editing_channel.set(None);
    signals.deleting_channel.set(None);
    signals.form_name.set(String::new());
    signals.form_protocol.set("openai".to_string());
    signals.form_base_url.set(String::new());
    signals.form_api_key.set(String::new());
    signals.form_models.set(String::new());
    signals.form_capabilities.set(Vec::new());
    signals.form_weight.set("1".to_string());
    signals.form_is_active.set(true);
    signals.form_tenant_id.set(String::new());
    signals.tenant_dropdown_open.set(false);
    signals.submitting.set(false);
    signals.form_error.set(None);
}

/// 仅保留租户 ID 前 8 位，超出部分以省略号收尾（如 `896297c7...`）。
fn short_tenant_id(id: &str) -> String {
    if id.chars().count() > 8 {
        let head: String = id.chars().take(8).collect();
        format!("{head}...")
    } else {
        id.to_string()
    }
}

#[allow(clippy::too_many_arguments)]
fn render_modal(
    t: &'static Translations,
    kind: ModalKind,
    form_error: Option<String>,
    submitting: bool,
    editing: Option<ChannelResponse>,
    deleting: Option<ChannelResponse>,
    signals: ChannelsSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
    actor_is_system: bool,
    actor_tenant_id: String,
    available_tenants: Signal<Vec<TenantResponse>>,
) -> Element {
    if kind == ModalKind::None {
        return VNode::empty();
    }

    if kind == ModalKind::DeleteConfirm {
        return render_delete_confirm(
            t, form_error, deleting, submitting, signals, client, log_bus, auth, nav,
        );
    }

    render_form_modal(
        t,
        kind,
        form_error,
        submitting,
        editing,
        signals,
        client,
        log_bus,
        auth,
        nav,
        actor_is_system,
        actor_tenant_id,
        available_tenants,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_delete_confirm(
    t: &'static Translations,
    form_error: Option<String>,
    deleting: Option<ChannelResponse>,
    submitting: bool,
    signals: ChannelsSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
) -> Element {
    let message = deleting
        .as_ref()
        .map(|c| {
            tf(
                t.channels_confirm_delete_msg,
                &[("name", &c.name), ("id", &c.id)],
            )
        })
        .unwrap_or_else(|| t.channels_no_target.to_string());
    let confirm_title = t.channels_confirm_delete_title.to_string();

    let signals_for_cancel = signals;
    let on_cancel = move |_: MouseEvent| {
        if !*signals_for_cancel.submitting.read() {
            close_all(signals_for_cancel);
        }
    };
    let mut s_async = signals;
    let c_async = client;
    let b_async = log_bus;
    let a_async = auth;
    let on_confirm = move |_: MouseEvent| {
        if *s_async.submitting.read() {
            return;
        }
        let Some(c) = s_async.deleting_channel.cloned() else {
            return;
        };
        let target_id = c.id.clone();
        s_async.submitting.set(true);
        let client_async = c_async.clone();
        let bus_async = b_async;
        let mut s_inner = s_async;
        let auth_async = a_async.clone();
        let lang = use_context::<I18nContext>().lang();
        spawn(async move {
            let res = client_async.delete_channel(&target_id).await;
            s_inner.submitting.set(false);
            match res {
                Ok(_) => {
                    push_log_ok(
                        bus_async,
                        HttpMethod::Delete,
                        &format!("/api/channels/{target_id}"),
                    );
                    if *s_inner.modal_kind.read() == ModalKind::DeleteConfirm {
                        s_inner.modal_kind.set(ModalKind::None);
                        s_inner.deleting_channel.set(None);
                        s_inner.form_error.set(None);
                    }
                    s_inner.list_version.with_mut(|v| *v += 1);
                }
                Err(err) => {
                    if crate::api::handle_unauth(&err, auth_async, nav, bus_async).await {
                        return;
                    }
                    push_log_err(
                        bus_async,
                        HttpMethod::Delete,
                        &format!("/api/channels/{target_id}"),
                        &err,
                    );
                    s_inner.form_error.set(Some(humanize_error(
                        &err,
                        ErrorContext::ChannelManagement,
                        lang,
                    )));
                }
            }
        });
    };

    rsx! {
        ConfirmDialog {
            open: true,
            title: confirm_title,
            message,
            danger: true,
            loading: submitting,
            confirm_label: t.channels_confirm_delete_btn.to_string(),
            on_confirm,
            on_cancel,
        }
        if let Some(err) = form_error.as_ref() {
            p { class: "ains-form-error", "{err}" }
        }
    }
}

fn resolve_channel_tenant_id(
    actor_is_system: bool,
    actor_tenant_id: &str,
    form_tenant_id: &str,
) -> Option<String> {
    if !actor_is_system {
        return None;
    }
    let tid = form_tenant_id.trim();
    if tid.is_empty() {
        Some(actor_tenant_id.to_string())
    } else {
        Some(tid.to_string())
    }
}

#[allow(clippy::too_many_arguments)]
fn render_form_modal(
    t: &'static Translations,
    kind: ModalKind,
    form_error: Option<String>,
    submitting: bool,
    editing: Option<ChannelResponse>,
    mut signals: ChannelsSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
    actor_is_system: bool,
    actor_tenant_id: String,
    available_tenants: Signal<Vec<TenantResponse>>,
) -> Element {
    let is_create = kind == ModalKind::Create;
    let title_str = if is_create {
        t.channels_modal_create_title
    } else {
        t.channels_modal_edit_title
    };
    let submit_label = if is_create {
        t.channels_modal_create_submit
    } else {
        t.channels_modal_edit_submit
    };

    // Pre-extract all translations to owned Strings
    let name_empty = t.channels_name_empty.to_string();
    let base_url_empty = t.channels_base_url_empty.to_string();
    let api_key_empty = t.channels_api_key_empty.to_string();
    let models_empty = t.channels_models_empty.to_string();
    let capabilities_empty = t.channels_capabilities_empty.to_string();
    let no_target_id = t.channels_modal_no_target_id.to_string();
    let form_name_label = t.channels_form_name_label.to_string();
    let form_name_placeholder = t.channels_form_name_placeholder.to_string();
    let form_protocol_label = t.channels_form_protocol_label.to_string();
    let form_protocol_openai = t.channels_form_protocol_openai.to_string();
    let form_protocol_anthropic = t.channels_form_protocol_anthropic.to_string();
    let form_base_url_label = t.channels_form_base_url_label.to_string();
    let form_base_url_placeholder = t.channels_form_base_url_placeholder.to_string();
    let form_api_key_label = t.channels_form_api_key_label.to_string();
    let form_api_key_placeholder = t.channels_form_api_key_placeholder.to_string();
    let form_models_label = t.channels_form_models_label.to_string();
    let form_models_placeholder = t.channels_form_models_placeholder.to_string();
    let form_capabilities_label = t.channels_form_capabilities_label.to_string();
    let form_weight_label = t.channels_form_weight_label.to_string();
    let weight_invalid = t.channels_weight_invalid.to_string();
    let form_is_active_label = t.channels_form_is_active_label.to_string();
    let form_tenant_id_label = t.channels_form_tenant_id_label.to_string();
    let form_api_key_hint = if is_create {
        t.channels_form_api_key_hint_create.to_string()
    } else {
        t.channels_form_api_key_hint_edit.to_string()
    };

    let signals_for_close = signals;
    let on_close = move |_: MouseEvent| {
        if !*signals_for_close.submitting.read() {
            close_all(signals_for_close);
        }
    };

    // Protocol toggle
    let mut sig_proto = signals;
    let pick_openai = move |_: MouseEvent| sig_proto.form_protocol.set("openai".to_string());
    let pick_anthropic = move |_: MouseEvent| sig_proto.form_protocol.set("anthropic".to_string());

    let editing_for_submit = editing.clone();
    let mut signals_for_submit = signals;
    let auth_for_submit = auth.clone();
    let client_for_submit = client.clone();
    let actor_tenant_id_for_submit = actor_tenant_id.clone();
    let on_submit = move |_: MouseEvent| {
        if *signals_for_submit.submitting.read() {
            return;
        }
        let name = signals_for_submit.form_name.cloned();
        let protocol = signals_for_submit.form_protocol.cloned();
        let base_url = signals_for_submit.form_base_url.cloned();
        let api_key = signals_for_submit.form_api_key.cloned();
        let models_str = signals_for_submit.form_models.cloned();
        let capabilities = signals_for_submit.form_capabilities.cloned();
        let weight_str = signals_for_submit.form_weight.cloned();
        let is_active = *signals_for_submit.form_is_active.read();
        let tenant_id = signals_for_submit.form_tenant_id.cloned();
        let editing_id = editing_for_submit.as_ref().map(|c| c.id.clone());
        let kind_now = kind;

        // Validation
        if name.trim().is_empty() {
            signals_for_submit.form_error.set(Some(name_empty.clone()));
            return;
        }
        if base_url.trim().is_empty() {
            signals_for_submit
                .form_error
                .set(Some(base_url_empty.clone()));
            return;
        }
        if is_create && api_key.trim().is_empty() {
            signals_for_submit
                .form_error
                .set(Some(api_key_empty.clone()));
            return;
        }
        if models_str.trim().is_empty() {
            signals_for_submit
                .form_error
                .set(Some(models_empty.clone()));
            return;
        }
        if capabilities.is_empty() {
            signals_for_submit
                .form_error
                .set(Some(capabilities_empty.clone()));
            return;
        }
        let weight: i32 = match weight_str.parse() {
            Ok(v) if v >= 1 => v,
            Ok(_) => {
                signals_for_submit.form_error.set(Some(format!(
                    "{}: weight '{}' must be at least 1",
                    weight_invalid, weight_str
                )));
                return;
            }
            Err(_) => {
                let hint = if let Ok(v) = weight_str.trim().parse::<i64>() {
                    if v < 0 {
                        format!(
                            "negative values are not allowed, got '{}'",
                            weight_str.trim()
                        )
                    } else {
                        format!(
                            "value '{}' is too large (max {})",
                            weight_str.trim(),
                            i32::MAX
                        )
                    }
                } else {
                    format!("got '{}'", weight_str)
                };
                signals_for_submit
                    .form_error
                    .set(Some(format!("{}: {}", weight_invalid, hint)));
                return;
            }
        };
        let models: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            models_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && seen.insert(s.clone()))
                .collect()
        };

        let client_async = client_for_submit.clone();
        let bus_async = log_bus;
        let mut s_async = signals_for_submit;
        let auth_async = auth_for_submit.clone();
        let no_target_id_async = no_target_id.clone();
        let lang = use_context::<I18nContext>().lang();
        signals_for_submit.submitting.set(true);
        signals_for_submit.form_error.set(None);
        // Clone before spawn to avoid consuming actor_tenant_id_for_submit, which would
        // make the outer on_submit closure FnOnce (disallowed by Button's FnMut onclick).
        let tenant_id_for_spawn = actor_tenant_id_for_submit.clone();
        spawn(async move {
            let res = match kind_now {
                ModalKind::Create => {
                    let input = client_api::CreateChannelRequest {
                        name,
                        protocol_type: protocol,
                        models,
                        capabilities,
                        api_key,
                        base_url,
                        weight,
                        is_active,
                        // 在异步闭包中重新读取 auth 状态，而非依赖外部捕获的 actor_is_system，
                        // 以防角色在表单填写期间发生变化（防御深度）。
                        tenant_id: {
                            let is_still_system = auth_async
                                .user
                                .read()
                                .as_ref()
                                .map(|u| u.is_system())
                                .unwrap_or(false);
                            resolve_channel_tenant_id(
                                is_still_system,
                                &tenant_id_for_spawn,
                                &tenant_id,
                            )
                        },
                    };
                    let r = client_async.create_channel(input).await;
                    if r.is_ok() {
                        push_log_ok(bus_async, HttpMethod::Post, "/api/channels");
                    }
                    r
                }
                ModalKind::Edit => {
                    let Some(ref id) = editing_id else {
                        s_async.form_error.set(Some(no_target_id_async));
                        s_async.submitting.set(false);
                        return;
                    };
                    let input = client_api::UpdateChannelRequest {
                        name: Some(name),
                        protocol_type: Some(protocol),
                        models: Some(models),
                        capabilities: Some(capabilities),
                        api_key: if api_key.trim().is_empty() {
                            None
                        } else {
                            Some(api_key)
                        },
                        base_url: Some(base_url),
                        is_active: Some(is_active),
                        weight: Some(weight),
                        // 与 Create 路径保持一致：在异步闭包中重新读取 auth 状态，
                        // 而非依赖外部捕获的 actor_is_system，以防角色在表单填写
                        // 期间发生变化（防御深度）。
                        tenant_id: {
                            let is_still_system = auth_async
                                .user
                                .read()
                                .as_ref()
                                .map(|u| u.is_system())
                                .unwrap_or(false);
                            if is_still_system {
                                Some(tenant_id)
                            } else {
                                None
                            }
                        },
                    };
                    let r = client_async.update_channel(id, input).await;
                    if r.is_ok() {
                        push_log_ok(bus_async, HttpMethod::Put, &format!("/api/channels/{}", id));
                    }
                    r
                }
                _ => unreachable!(),
            };
            s_async.submitting.set(false);
            match res {
                Ok(_) => {
                    if *s_async.modal_kind.read() == kind_now {
                        close_all(s_async);
                    }
                    s_async.list_version.with_mut(|v| *v += 1);
                }
                Err(err) => {
                    if crate::api::handle_unauth(&err, auth_async, nav, bus_async).await {
                        return;
                    }
                    let log_method = if kind_now == ModalKind::Create {
                        HttpMethod::Post
                    } else {
                        HttpMethod::Put
                    };
                    let log_path = if kind_now == ModalKind::Create {
                        "/api/channels".to_string()
                    } else {
                        format!("/api/channels/{}", editing_id.unwrap_or_default())
                    };
                    push_log_err(bus_async, log_method, &log_path, &err);
                    s_async.form_error.set(Some(humanize_error(
                        &err,
                        ErrorContext::ChannelManagement,
                        lang,
                    )));
                }
            }
        });
    };

    let protocol_now = signals.form_protocol.cloned();
    let capabilities_now = signals.form_capabilities.cloned();
    let is_active_now = *signals.form_is_active.read();
    let tenants_snapshot = available_tenants.cloned();
    let active_tenants: Vec<&TenantResponse> = tenants_snapshot
        .iter()
        .filter(|t| t.status == "active")
        .collect();

    // 自定义租户下拉的展开态与当前选中项标题（从全量租户中解析，
    // 确保即使当前租户已禁用也能在触发器上显示名称）。
    let tenant_dropdown_open = *signals.tenant_dropdown_open.read();
    let selected_tenant_id = signals.form_tenant_id.read().clone();
    let selected_tenant_label = tenants_snapshot
        .iter()
        .find(|t| t.id == selected_tenant_id)
        .map(|t| format!("{} ({})", t.name, short_tenant_id(&t.id)))
        .unwrap_or_else(|| {
            if selected_tenant_id.is_empty() {
                "—".to_string()
            } else {
                short_tenant_id(&selected_tenant_id)
            }
        });

    // 租户下拉的「无限滚动」加载：每次滚到接近底部时拉取下一页（100 条），
    // 直到 has_more 为 false。loading 标志防止并发重复拉取。
    let mut load_more_tenants = {
        let client = client.clone();
        let auth_for_more = auth.clone();
        let bus = log_bus;
        let mut at = available_tenants;
        let mut next_page = signals.tenant_next_page;
        let mut has_more = signals.tenant_has_more;
        let mut loading = signals.tenant_loading;
        move || {
            if *loading.read() || !*has_more.read() {
                return;
            }
            let page = *next_page.read();
            loading.set(true);
            let client = client.clone();
            let auth_inner = auth_for_more.clone();
            spawn(async move {
                match client.list_tenants(page, 100).await {
                    Ok(data) => {
                        let more = data.page < data.total_pages;
                        at.with_mut(|v| v.extend(data.items));
                        next_page.set(page + 1);
                        has_more.set(more);
                        loading.set(false);
                    }
                    Err(err) => {
                        if crate::api::handle_unauth(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/tenants", &err);
                        loading.set(false);
                    }
                }
            });
        }
    };

    rsx! {
        Modal {
            title: title_str.to_string(),
            on_close,
            open: true,
            disable_backdrop: submitting,
            disable_close: submitting,
            div {
                class: "ains-form-stack",
                // 点击下拉以外的任意空白/字段区域时关闭租户下拉（不影响模态框）。
                // 无需覆盖层，因此不会遮挡模态框右侧滚动条。
                onclick: move |_: MouseEvent| {
                    let mut open_sig = signals.tenant_dropdown_open;
                    if *open_sig.read() {
                        open_sig.set(false);
                    }
                },
                if let Some(err) = form_error.as_ref() {
                    p { class: "ains-form-error", "{err}" }
                }
                TextInput {
                    label: form_name_label.clone(),
                    placeholder: Some(form_name_placeholder.clone()),
                    value: signals.form_name,
                    required: true,
                    disabled: submitting,
                    name: Some("name".to_string()),
                }

                // Protocol Type
                div { class: "ains-form-field",
                    label { class: "ains-form-label", "{form_protocol_label}" }
                    div { class: "ains-form-pill-group",
                        button {
                            r#type: "button",
                            class: if protocol_now == "openai" { "ains-form-pill ains-form-pill--active" } else { "ains-form-pill" },
                            onclick: pick_openai,
                            "{form_protocol_openai}"
                        }
                        button {
                            r#type: "button",
                            class: if protocol_now == "anthropic" { "ains-form-pill ains-form-pill--active" } else { "ains-form-pill" },
                            onclick: pick_anthropic,
                            "{form_protocol_anthropic}"
                        }
                    }
                }

                TextInput {
                    label: form_base_url_label.clone(),
                    placeholder: Some(form_base_url_placeholder.clone()),
                    value: signals.form_base_url,
                    required: true,
                    disabled: submitting,
                    name: Some("base_url".to_string()),
                }

                TextInput {
                    label: form_api_key_label.clone(),
                    placeholder: Some(form_api_key_placeholder.clone()),
                    value: signals.form_api_key,
                    input_type: InputType::Password,
                    required: is_create,
                    disabled: submitting,
                    name: Some("api_key".to_string()),
                    hint: Some(form_api_key_hint.clone()),
                }

                TextInput {
                    label: form_models_label.clone(),
                    placeholder: Some(form_models_placeholder.clone()),
                    value: signals.form_models,
                    required: true,
                    disabled: submitting,
                    name: Some("models".to_string()),
                    hint: None,
                }

                // Capabilities multi-select
                div { class: "ains-form-field",
                    label { class: "ains-form-label", "{form_capabilities_label}" }
                    div { class: "ains-form-pill-group",
                        for (cap_value , cap_label) in ALL_CAPABILITIES.iter() {
                            {
                                let cap_val = cap_value.to_string();
                                let is_selected =
                                    capabilities_now.contains(&cap_val);
                                let class = if is_selected {
                                    "ains-form-pill ains-form-pill--active"
                                } else {
                                    "ains-form-pill"
                                };
                                let cap_for_toggle = cap_val.clone();
                                let mut sig_toggle = signals;
                                rsx! {
                                    button {
                                        r#type: "button",
                                        class,
                                        onclick: move |_: MouseEvent| {
                                            let c = cap_for_toggle.clone();
                                            sig_toggle
                                                .form_capabilities
                                                .with_mut(|caps| {
                                                    if let Some(pos) = caps.iter().position(|x| x == &c) {
                                                        caps.remove(pos);
                                                    } else {
                                                        caps.push(c);
                                                    }
                                                });
                                        },
                                        "{cap_label}"
                                    }
                                }
                            }
                        }
                    }
                }

                TextInput {
                    label: form_weight_label.clone(),
                    placeholder: Some("1".to_string()),
                    value: signals.form_weight,
                    input_type: InputType::Text,
                    required: false,
                    disabled: submitting,
                    name: Some("weight".to_string()),
                    hint: None,
                }

                // is_active checkbox (Edit only; Create defaults to true)
                if !is_create {
                    div { class: "ains-form-field",
                        label { class: "ains-form-checkbox-label",
                            input {
                                r#type: "checkbox",
                                checked: is_active_now,
                                disabled: submitting,
                                onchange: move |evt| {
                                    signals.form_is_active.set(evt.checked());
                                },
                            }
                            span { "{form_is_active_label}" }
                        }
                    }
                }

                // Tenant 下拉选择（system only）——自绘下拉，触发器与弹层均与
                // Modal 内其它字段对齐；选项仅展示租户 ID 前 8 位。
                if actor_is_system {
                    div { class: "ains-input",
                        label { class: "ains-input__label", "{form_tenant_id_label}" }
                        div {
                            class: if tenant_dropdown_open { "ains-select ains-select--open" } else { "ains-select" },
                            // 阻止下拉内部（触发器/弹层/选项）的点击冒泡到外层关闭逻辑，
                            // 确保点击触发器/选项本身不会触发“点击空白处关闭”。
                            onclick: move |e: MouseEvent| e.stop_propagation(),
                            button {
                                r#type: "button",
                                class: "ains-select__trigger",
                                disabled: submitting,
                                onclick: move |_: MouseEvent| {
                                    let mut open_sig = signals.tenant_dropdown_open;
                                    let cur = *open_sig.read();
                                    open_sig.set(!cur);
                                },
                                span { class: "ains-select__value", "{selected_tenant_label}" }
                                ChevronDown { class: "ains-select__chevron" }
                            }
                            if tenant_dropdown_open {
                                div {
                                    class: "ains-select__panel",
                                    id: "{TENANT_PANEL_ID}",
                                    onscroll: move |_| {
                                        if super::element_near_bottom(TENANT_PANEL_ID) {
                                            load_more_tenants();
                                        }
                                    },
                                    for tenant in &active_tenants {
                                        {
                                            let tid = tenant.id.clone();
                                            let is_sel = tenant.id == selected_tenant_id;
                                            let opt_label =
                                                format!("{} ({})", tenant.name, short_tenant_id(&tenant.id));
                                            let mut tid_sig = signals.form_tenant_id;
                                            let mut open_sig = signals.tenant_dropdown_open;
                                            rsx! {
                                                button {
                                                    r#type: "button",
                                                    class: if is_sel { "ains-select__option ains-select__option--active" } else { "ains-select__option" },
                                                    onclick: move |_: MouseEvent| {
                                                        tid_sig.set(tid.clone());
                                                        open_sig.set(false);
                                                    },
                                                    "{opt_label}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                Button {
                    button_type: ButtonType::Submit,
                    full_width: true,
                    disabled: submitting,
                    loading: submitting,
                    onclick: on_submit,
                    "{submit_label}"
                }
            }
        }
    }
}

fn build_columns(t: &'static Translations) -> Vec<Column> {
    vec![
        Column::new(t.channels_column_name).align(Align::Left),
        Column::new(t.channels_column_tenant)
            .width("w-40")
            .align(Align::Left),
        Column::new(t.channels_column_protocol)
            .width("w-24")
            .align(Align::Center),
        Column::new(t.channels_column_base_url)
            .width("w-48")
            .align(Align::Left),
        Column::new(t.channels_column_status)
            .width("w-20")
            .align(Align::Center),
        Column::new(t.channels_column_weight)
            .width("w-16")
            .align(Align::Right),
        Column::new(t.channels_column_capabilities)
            .width("w-48")
            .align(Align::Left),
        Column::new(t.channels_column_created)
            .width("w-36")
            .align(Align::Left),
        Column::new(t.channels_column_actions)
            .width("w-36")
            .align(Align::Center),
    ]
}

fn format_dt(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_tenant_id_non_system_returns_none() {
        assert_eq!(
            resolve_channel_tenant_id(false, "default", "tenant-x"),
            None
        );
        assert_eq!(resolve_channel_tenant_id(false, "", ""), None);
    }

    #[test]
    fn resolve_tenant_id_system_uses_form_value() {
        assert_eq!(
            resolve_channel_tenant_id(true, "system-tenant", "target-tenant"),
            Some("target-tenant".to_string())
        );
    }

    #[test]
    fn resolve_tenant_id_system_empty_form_uses_actor_tenant() {
        assert_eq!(
            resolve_channel_tenant_id(true, "system-tenant", ""),
            Some("system-tenant".to_string())
        );
    }

    #[test]
    fn resolve_tenant_id_trims_form_value() {
        assert_eq!(
            resolve_channel_tenant_id(true, "default", "  tenant-x  "),
            Some("tenant-x".to_string())
        );
    }
}
