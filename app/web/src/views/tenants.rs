//! Tenant 管理视图（admin/system）。
//!
//! 参照 `users.rs` 的架构模式，完整接入 `client-api` 的 list / create / update / delete。

use std::cell::Cell;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use client_api::TenantResponse;
use dioxus::prelude::dioxus_router::Navigator;
use dioxus::prelude::*;
use dioxus_icons::lucide::{LoaderCircle, Pencil, Plus, ShieldHalf, Trash2, TriangleAlert};

use ui::{
    Align, Badge, BadgeVariant, Button, ButtonType, Column, DataTable, I18nContext, Modal,
    TextInput, Translations, tf,
};

use crate::api::{ErrorContext, humanize_error};
use crate::auth::AuthState;
use crate::components::{
    ConfirmDialog, HttpMethod, LogBus, SearchSignal, push_log_err, push_log_ok,
};

use super::{PaginationEntity, format_pagination_info};

/// Default tenant ID — MUST match the value seeded in `server/migrations/001_init.sql`.
/// If the server-side default tenant ID changes, this constant must be updated,
/// otherwise the frontend protection logic (edit/delete blocked for default tenant)
/// will silently fail to protect the actual default tenant.
const DEFAULT_TENANT_ID: &str = "default";

#[derive(Debug, Clone)]
enum ListState {
    Loading,
    Loaded(Vec<TenantResponse>),
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
struct TenantsSignals {
    modal_kind: Signal<ModalKind>,
    editing_tenant: Signal<Option<TenantResponse>>,
    deleting_tenant: Signal<Option<TenantResponse>>,
    form_name: Signal<String>,
    form_status: Signal<String>,
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
pub fn Tenants() -> Element {
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

    let signals = TenantsSignals {
        modal_kind: use_signal(|| ModalKind::None),
        editing_tenant: use_signal(|| Option::<TenantResponse>::None),
        deleting_tenant: use_signal(|| Option::<TenantResponse>::None),
        form_name: use_signal(String::new),
        form_status: use_signal(|| "active".to_string()),
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
                let res = client.list_tenants(current_page, current_per_page).await;
                if version_check() != version
                    || page() != current_page
                    || per_page() != current_per_page
                {
                    return;
                }
                match res {
                    Ok(page_data) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/tenants");
                        total_signal.set(page_data.total);
                        total_pages_signal.set(page_data.total_pages);
                        list.set(ListState::Loaded(page_data.items));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/tenants", &err);
                        list.set(ListState::Error(humanize_error(
                            &err,
                            ErrorContext::TenantManagement,
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
    let editing_snapshot = signals.editing_tenant.read().clone();
    let deleting_snapshot = signals.deleting_tenant.read().clone();

    let current_user = auth.user.read().as_ref().cloned();
    let actor_is_system = current_user
        .as_ref()
        .map(|u| u.is_system())
        .unwrap_or(false);

    let page_val = page();
    let per_page_val = per_page();
    let total_val = total();
    let total_pages_val = total_pages();

    let mut signals_for_open = signals;
    let open_create = move |_: MouseEvent| {
        signals_for_open.form_name.set(String::new());
        signals_for_open.form_status.set("active".to_string());
        signals_for_open.form_error.set(None);
        signals_for_open.editing_tenant.set(None);
        signals_for_open.modal_kind.set(ModalKind::Create);
    };

    rsx! {
        div { class: "ains-users",
            header { class: "ains-users__header",
                div { class: "ains-users__title-block",
                    h1 { class: "ains-users__title", "{t.tenants_title}" }
                    p { class: "ains-users__subtitle", "{t.tenants_subtitle}" }
                }
                div { class: "ains-users__header-actions",
                    span { class: "ains-users__guard-pill",
                        ShieldHalf {}
                        "{t.tenants_guard_pill}"
                    }
                    if actor_is_system {
                        Button { onclick: open_create,
                            Plus {}
                            "{t.tenants_create_btn}"
                        }
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
                    PaginationState {
                        page: page_val,
                        per_page: per_page_val,
                        total: total_val,
                        total_pages: total_pages_val,
                        page_signal: page,
                        per_page_signal: per_page,
                    },
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
                )
            }
        }
    }
}

fn render_table(
    t: &'static Translations,
    list_snapshot: ListState,
    search_text: String,
    signals: TenantsSignals,
    actor_is_system: bool,
    pagination: PaginationState,
) -> Element {
    match list_snapshot {
        ListState::Loading => rsx! {
            div { class: "ains-users__status",
                LoaderCircle { class: "ains-btn__spinner" }
                "{t.tenants_loading}"
            }
        },
        ListState::Error(msg) => rsx! {
            div { class: "ains-users__status ains-users__status--error",
                TriangleAlert {}
                "{msg}"
            }
        },
        ListState::Loaded(items) => {
            let filtered: Vec<TenantResponse> = if search_text.is_empty() {
                items
            } else {
                let q = search_text.to_lowercase();
                items
                    .into_iter()
                    .filter(|u| {
                        u.name.to_lowercase().contains(&q) || u.id.to_lowercase().contains(&q)
                    })
                    .collect()
            };
            let columns = build_columns(t);
            let rows: Vec<Element> = filtered
                .into_iter()
                .map(|tenant| row_element(t, tenant, signals, actor_is_system))
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
            let pagination_info =
                format_pagination_info(t, PaginationEntity::Tenants, total, page, total_pages);

            rsx! {
                div { class: "ains-users__table-wrapper",
                    DataTable {
                        columns,
                        rows,
                        empty: Some(rsx! { "{t.tenants_empty}" }),
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

fn row_element(
    t: &'static Translations,
    tenant: TenantResponse,
    signals: TenantsSignals,
    actor_is_system: bool,
) -> Element {
    let id = tenant.id.clone();
    let name = tenant.name.clone();
    let status = tenant.status.clone();
    let created = tenant.created_at;
    let is_default = id == DEFAULT_TENANT_ID;
    let is_active = status == "active";

    // Only system can edit/delete tenants. Default tenant is protected.
    let can_edit = actor_is_system && !is_default;
    let can_delete = actor_is_system && !is_default;

    let t_for_edit = tenant.clone();
    let t_for_delete = tenant.clone();
    let mut s_edit = signals;
    let mut s_delete = signals;
    let edit_handler = move |_: MouseEvent| {
        s_edit.form_name.set(t_for_edit.name.clone());
        s_edit.form_status.set(t_for_edit.status.clone());
        s_edit.form_error.set(None);
        s_edit.editing_tenant.set(Some(t_for_edit.clone()));
        s_edit.modal_kind.set(ModalKind::Edit);
    };
    let delete_handler = move |_: MouseEvent| {
        s_delete.form_error.set(None);
        s_delete.deleting_tenant.set(Some(t_for_delete.clone()));
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
            td { class: "ains-table__mono ains-table__align--right", "{tenant.user_count}" }
            td { class: "ains-table__mono ains-table__align--right", "{tenant.channel_count}" }
            td {
                if is_active {
                    Badge { variant: BadgeVariant::User, "{t.tenants_badge_active}" }
                } else {
                    Badge { variant: BadgeVariant::Admin, "{t.tenants_badge_disabled}" }
                }
            }
            td { class: "ains-table__mono", "{format_dt(&created)}" }
            td {
                if is_default {
                    div { class: "ains-table__row-actions",
                        span {
                            class: "ains-table__protected",
                            title: "{t.tenants_default_protected}",
                            ShieldHalf {}
                            "{t.tenants_protected_label}"
                        }
                    }
                } else if !actor_is_system {
                    div { class: "ains-table__row-actions",
                        span { class: "ains-table__protected",
                            ShieldHalf {}
                            "{t.tenants_no_permission}"
                        }
                    }
                } else {
                    div { class: "ains-table__row-actions",
                        if can_edit {
                            button {
                                class: "ains-table__action",
                                title: "{t.tenants_edit_title}",
                                onclick: edit_handler,
                                Pencil {}
                            }
                        }
                        if can_delete {
                            button {
                                class: "ains-table__action ains-table__action--danger",
                                title: "{t.tenants_delete_title}",
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

fn close_all(mut signals: TenantsSignals) {
    signals.modal_kind.set(ModalKind::None);
    signals.editing_tenant.set(None);
    signals.deleting_tenant.set(None);
    signals.form_name.set(String::new());
    signals.form_status.set("active".to_string());
    signals.submitting.set(false);
    signals.form_error.set(None);
}

#[allow(clippy::too_many_arguments)]
fn render_modal(
    t: &'static Translations,
    kind: ModalKind,
    form_error: Option<String>,
    submitting: bool,
    editing: Option<TenantResponse>,
    deleting: Option<TenantResponse>,
    signals: TenantsSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
) -> Element {
    if kind == ModalKind::None {
        return VNode::empty();
    }

    if kind == ModalKind::DeleteConfirm {
        return render_delete_confirm(t, deleting, submitting, signals, client, log_bus, auth, nav);
    }

    render_form_modal(
        t, kind, form_error, submitting, editing, signals, client, log_bus, auth, nav,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_delete_confirm(
    t: &'static Translations,
    deleting: Option<TenantResponse>,
    submitting: bool,
    signals: TenantsSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
) -> Element {
    let message = deleting
        .as_ref()
        .map(|tenant| {
            tf(
                t.tenants_confirm_delete_msg,
                &[("name", &tenant.name), ("id", &tenant.id)],
            )
        })
        .unwrap_or_else(|| t.tenants_no_target.to_string());
    let confirm_delete_title = t.tenants_confirm_delete_title.to_string();

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
        let Some(tenant) = s_async.deleting_tenant.cloned() else {
            return;
        };
        // 在确认时重新读取 auth 状态，而非依赖闭包创建时捕获的 actor_is_system，
        // 以防角色在 Modal 打开期间发生变化（防御深度）。
        let is_still_system = a_async
            .user
            .read()
            .as_ref()
            .map(|u| u.is_system())
            .unwrap_or(false);
        if !is_still_system {
            s_async
                .form_error
                .set(Some(t.tenants_no_permission.to_string()));
            return;
        }
        if tenant.id == DEFAULT_TENANT_ID {
            s_async
                .form_error
                .set(Some(t.tenants_default_protected.to_string()));
            return;
        }
        let target_id = tenant.id.clone();
        s_async.submitting.set(true);
        let client_async = c_async.clone();
        let bus_async = b_async;
        let mut s_inner = s_async;
        let auth_async = a_async.clone();
        let lang = use_context::<I18nContext>().lang();
        spawn(async move {
            let res = client_async.delete_tenant(&target_id).await;
            s_inner.submitting.set(false);
            match res {
                Ok(_) => {
                    push_log_ok(
                        bus_async,
                        HttpMethod::Delete,
                        &format!("/api/tenants/{target_id}"),
                    );
                    if *s_inner.modal_kind.read() == ModalKind::DeleteConfirm {
                        s_inner.modal_kind.set(ModalKind::None);
                        s_inner.deleting_tenant.set(None);
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
                        &format!("/api/tenants/{target_id}"),
                        &err,
                    );
                    s_inner.form_error.set(Some(humanize_error(
                        &err,
                        ErrorContext::TenantManagement,
                        lang,
                    )));
                }
            }
        });
    };

    rsx! {
        ConfirmDialog {
            open: true,
            title: confirm_delete_title,
            message,
            danger: true,
            loading: submitting,
            confirm_label: t.tenants_confirm_delete_btn.to_string(),
            on_confirm,
            on_cancel,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_form_modal(
    t: &'static Translations,
    kind: ModalKind,
    form_error: Option<String>,
    submitting: bool,
    editing: Option<TenantResponse>,
    signals: TenantsSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
) -> Element {
    let title_str = if kind == ModalKind::Create {
        t.tenants_modal_create_title
    } else {
        t.tenants_modal_edit_title
    };
    let submit_label = if kind == ModalKind::Create {
        t.tenants_modal_create_submit
    } else {
        t.tenants_modal_edit_submit
    };

    let name_empty = t.tenants_name_empty.to_string();
    let no_target_id = t.tenants_modal_no_target_id.to_string();
    let form_name_label = t.tenants_form_name_label.to_string();
    let form_name_placeholder = t.tenants_form_name_placeholder.to_string();
    let form_status_label = t.tenants_form_status_label.to_string();
    let form_status_active = t.tenants_form_status_active.to_string();
    let form_status_disabled = t.tenants_form_status_disabled.to_string();

    let signals_for_close = signals;
    let on_close = move |_: MouseEvent| {
        if !*signals_for_close.submitting.read() {
            close_all(signals_for_close);
        }
    };
    let mut signals_for_status = signals;
    let pick_active = move |_: MouseEvent| signals_for_status.form_status.set("active".to_string());
    let pick_disabled =
        move |_: MouseEvent| signals_for_status.form_status.set("disabled".to_string());

    let editing_for_submit = editing.clone();
    let mut signals_for_submit = signals;
    let auth_for_submit = auth.clone();
    let client_for_submit = client.clone();
    let on_submit = move |_: MouseEvent| {
        if *signals_for_submit.submitting.read() {
            return;
        }
        let name = signals_for_submit.form_name.cloned();
        let status = signals_for_submit.form_status.cloned();
        let editing_id = editing_for_submit.as_ref().map(|t| t.id.clone());
        let kind_now = kind;
        if name.trim().is_empty() {
            signals_for_submit.form_error.set(Some(name_empty.clone()));
            return;
        }
        let client_async = client_for_submit.clone();
        let bus_async = log_bus;
        let mut s_async = signals_for_submit;
        let auth_async = auth_for_submit.clone();
        let no_target_id_async = no_target_id.clone();
        let lang = use_context::<I18nContext>().lang();
        signals_for_submit.submitting.set(true);
        signals_for_submit.form_error.set(None);
        spawn(async move {
            let res = match kind_now {
                ModalKind::Create => {
                    let r = client_async.create_tenant(name).await;
                    if r.is_ok() {
                        push_log_ok(bus_async, HttpMethod::Post, "/api/tenants");
                    }
                    r
                }
                ModalKind::Edit => {
                    let Some(ref id) = editing_id else {
                        s_async.form_error.set(Some(no_target_id_async));
                        s_async.submitting.set(false);
                        return;
                    };
                    let r = client_async
                        .update_tenant(id, Some(name), Some(status))
                        .await;
                    if r.is_ok() {
                        push_log_ok(bus_async, HttpMethod::Put, &format!("/api/tenants/{}", id));
                    }
                    r
                }
                ModalKind::DeleteConfirm | ModalKind::None => unreachable!(),
            };
            s_async.submitting.set(false);
            match res {
                Ok(_) => {
                    if *s_async.modal_kind.read() == kind_now {
                        s_async.modal_kind.set(ModalKind::None);
                        s_async.editing_tenant.set(None);
                        s_async.form_name.set(String::new());
                        s_async.form_status.set("active".to_string());
                        s_async.form_error.set(None);
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
                        "/api/tenants".to_string()
                    } else {
                        format!("/api/tenants/{}", editing_id.unwrap_or_default())
                    };
                    push_log_err(bus_async, log_method, &log_path, &err);
                    s_async.form_error.set(Some(humanize_error(
                        &err,
                        ErrorContext::TenantManagement,
                        lang,
                    )));
                }
            }
        });
    };

    let status_now = signals.form_status.cloned();
    let is_create = kind == ModalKind::Create;

    rsx! {
        Modal {
            title: title_str.to_string(),
            on_close,
            open: true,
            disable_backdrop: submitting,
            disable_close: submitting,
            div { class: "ains-form-stack",
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
                // Status toggle only in Edit mode (created tenants default to active)
                if !is_create {
                    div { class: "ains-form-field",
                        label { class: "ains-form-label", "{form_status_label}" }
                        div { class: "ains-form-pill-group",
                            button {
                                r#type: "button",
                                class: if status_now == "active" { "ains-form-pill ains-form-pill--active" } else { "ains-form-pill" },
                                onclick: pick_active,
                                "{form_status_active}"
                            }
                            button {
                                r#type: "button",
                                class: if status_now == "disabled" { "ains-form-pill ains-form-pill--active" } else { "ains-form-pill" },
                                onclick: pick_disabled,
                                "{form_status_disabled}"
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
        Column::new(t.tenants_column_name).align(Align::Left),
        Column::new(t.tenants_column_users)
            .width("w-16")
            .align(Align::Right),
        Column::new(t.tenants_column_channels)
            .width("w-16")
            .align(Align::Right),
        Column::new(t.tenants_column_status)
            .width("w-24")
            .align(Align::Center),
        Column::new(t.tenants_column_created)
            .width("w-40")
            .align(Align::Left),
        Column::new(t.tenants_column_actions)
            .width("w-44")
            .align(Align::Center),
    ]
}

fn format_dt(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M").to_string()
}
