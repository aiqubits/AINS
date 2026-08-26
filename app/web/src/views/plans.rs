//! 套餐管理视图（admin/system）。
//!
//! 参照 `tenants.rs` 的架构模式，完整接入 `client-api` 的 list / create / update / delete。
//! 套餐为租户隔离资源：system 可见全部租户（含租户列与创建时的租户选择），
//! admin 由服务端强制限定在自身租户。

use std::cell::Cell;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use client_api::{CreatePlanRequest, PlanResponse, TenantResponse, UpdatePlanRequest};
use dioxus::prelude::dioxus_router::Navigator;
use dioxus::prelude::*;
use dioxus_icons::lucide::{LoaderCircle, Pencil, Plus, Trash2, TriangleAlert};

use ui::{
    Align, Badge, BadgeVariant, Button, ButtonType, Column, DataTable, I18nContext, InputType,
    Modal, TextInput, Translations, tf,
};

use crate::api::{ErrorContext, humanize_error};
use crate::auth::AuthState;
use crate::balance::{format_balance, parse_display_amount};
use crate::components::{
    ConfirmDialog, HttpMethod, LogBus, SearchSignal, push_log_err, push_log_ok,
};

use super::{
    format_purchase_limit,
    tenant_select::{TenantSelectView, load_tenant_page, render_tenant_select},
};

/// DOM id of the tenant dropdown scroll panel — used by the infinite-scroll
/// handler to read the panel's scroll position via `element_near_bottom`.
const TENANT_PANEL_ID: &str = "plan-tenant-dropdown-panel";
const MAX_PLAN_VALIDITY_DAYS: i32 = 36_500;
// Free plans are supported. `step="any"` avoids the browser rejecting values
// that the exact decimal parser and backend contract intentionally support.
const MIN_PLAN_PRICE: &str = "0";
const PLAN_PRICE_STEP: &str = "any";

#[derive(Debug, Clone)]
enum ListState {
    Loading,
    Loaded(Vec<PlanResponse>),
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
struct PlansSignals {
    modal_kind: Signal<ModalKind>,
    editing_plan: Signal<Option<PlanResponse>>,
    deleting_plan: Signal<Option<PlanResponse>>,
    form_tenant_id: Signal<String>,
    /// 自定义租户下拉是否展开（与用户管理对齐：原生 select 弹层无法与 Modal 风格对齐，故自绘）。
    tenant_dropdown_open: Signal<bool>,
    /// 租户下拉「无限滚动」分页状态：下一页页码、是否还有更多、是否正在加载。
    tenant_next_page: Signal<u64>,
    tenant_has_more: Signal<bool>,
    tenant_loading: Signal<bool>,
    form_name: Signal<String>,
    form_desc: Signal<String>,
    form_price: Signal<String>,
    form_calls: Signal<String>,
    form_purchase_limit: Signal<String>,
    form_validity: Signal<String>,
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
pub fn Plans() -> Element {
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
    let available_tenants = use_signal(Vec::<TenantResponse>::new);
    let SearchSignal(search_query) = use_context::<SearchSignal>();

    let signals = PlansSignals {
        modal_kind: use_signal(|| ModalKind::None),
        editing_plan: use_signal(|| Option::<PlanResponse>::None),
        deleting_plan: use_signal(|| Option::<PlanResponse>::None),
        form_tenant_id: use_signal(String::new),
        tenant_dropdown_open: use_signal(|| false),
        tenant_next_page: use_signal(|| 1u64),
        tenant_has_more: use_signal(|| false),
        tenant_loading: use_signal(|| false),
        form_name: use_signal(String::new),
        form_desc: use_signal(String::new),
        form_price: use_signal(String::new),
        form_calls: use_signal(String::new),
        form_purchase_limit: use_signal(String::new),
        form_validity: use_signal(String::new),
        form_status: use_signal(|| "active".to_string()),
        submitting: use_signal(|| false),
        form_error: use_signal(|| Option::<String>::None),
        list_version,
    };

    let current_user = auth.user.read().as_ref().cloned();
    let actor_is_system = current_user
        .as_ref()
        .map(|u| u.is_system())
        .unwrap_or(false);

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

    // system 角色预取租户列表供创建套餐时选择归属租户（与用户管理共用
    // load_tenant_page，内部全部 peek() 不会为本 effect 增加订阅）：
    // 首页 100 条 + 下拉「无限滚动」补页。错误显式上报，避免静默吞掉
    // 导致下拉为空；失败后展开下拉时会自动重试首页。
    {
        let client = auth.client.clone();
        let auth_for_tenants = auth.clone();
        use_effect(move || {
            if !actor_is_system {
                return;
            }
            load_tenant_page(
                client.clone(),
                auth_for_tenants.clone(),
                nav,
                log_bus,
                available_tenants,
                signals.tenant_next_page,
                signals.tenant_has_more,
                signals.tenant_loading,
            );
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
                let res = client
                    .list_plans(current_page, current_per_page, None)
                    .await;
                if version_check() != version
                    || page() != current_page
                    || per_page() != current_per_page
                {
                    return;
                }
                match res {
                    Ok(page_data) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/plans");
                        total_signal.set(page_data.total);
                        total_pages_signal.set(page_data.total_pages);
                        list.set(ListState::Loaded(page_data.items));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/plans", &err);
                        list.set(ListState::Error(humanize_error(
                            &err,
                            ErrorContext::PlanManagement,
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
    let editing_snapshot = signals.editing_plan.read().clone();
    let deleting_snapshot = signals.deleting_plan.read().clone();

    let page_val = page();
    let per_page_val = per_page();
    let total_val = total();
    let total_pages_val = total_pages();

    let mut signals_for_open = signals;
    let open_create = move |_: MouseEvent| {
        signals_for_open.form_tenant_id.set(String::new());
        signals_for_open.tenant_dropdown_open.set(false);
        signals_for_open.form_name.set(String::new());
        signals_for_open.form_desc.set(String::new());
        signals_for_open.form_price.set(String::new());
        signals_for_open.form_calls.set(String::new());
        signals_for_open.form_purchase_limit.set(String::new());
        signals_for_open.form_validity.set(String::new());
        signals_for_open.form_status.set("active".to_string());
        signals_for_open.form_error.set(None);
        signals_for_open.editing_plan.set(None);
        signals_for_open.modal_kind.set(ModalKind::Create);
    };

    rsx! {
        div { class: "ains-users",
            header { class: "ains-users__header",
                div { class: "ains-users__title-block",
                    h1 { class: "ains-users__title", "{t.plans_title}" }
                    p { class: "ains-users__subtitle", "{t.plans_subtitle}" }
                }
                div { class: "ains-users__header-actions",
                    Button { onclick: open_create,
                        Plus {}
                        "{t.plans_create_btn}"
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
                    actor_is_system,
                    available_tenants,
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
    signals: PlansSignals,
    actor_is_system: bool,
    pagination: PaginationState,
) -> Element {
    match list_snapshot {
        ListState::Loading => rsx! {
            div { class: "ains-users__status",
                LoaderCircle { class: "ains-btn__spinner" }
                "{t.plans_loading}"
            }
        },
        ListState::Error(msg) => rsx! {
            div { class: "ains-users__status ains-users__status--error",
                TriangleAlert {}
                "{msg}"
            }
        },
        ListState::Loaded(items) => {
            let filtered: Vec<PlanResponse> = if search_text.is_empty() {
                items
            } else {
                let q = search_text.to_lowercase();
                items
                    .into_iter()
                    .filter(|p| {
                        p.name.to_lowercase().contains(&q)
                            || p.description.to_lowercase().contains(&q)
                    })
                    .collect()
            };
            let columns = build_columns(t, actor_is_system);
            let rows: Vec<Element> = filtered
                .into_iter()
                .map(|plan| row_element(t, plan, signals, actor_is_system))
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
                        empty: Some(rsx! { "{t.plans_empty}" }),
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
    plan: PlanResponse,
    signals: PlansSignals,
    actor_is_system: bool,
) -> Element {
    let id = plan.id.clone();
    let name = plan.name.clone();
    let description = plan.description.clone();
    let tenant_label = plan
        .tenant_name
        .clone()
        .unwrap_or_else(|| plan.tenant_id.clone());
    let price_display = format_balance(plan.price);
    let is_active = plan.status == "active";
    let created = plan.created_at;

    let p_for_edit = plan.clone();
    let p_for_delete = plan.clone();
    let mut s_edit = signals;
    let mut s_delete = signals;
    let edit_handler = move |_: MouseEvent| {
        s_edit.form_tenant_id.set(p_for_edit.tenant_id.clone());
        s_edit.tenant_dropdown_open.set(false);
        s_edit.form_name.set(p_for_edit.name.clone());
        s_edit.form_desc.set(p_for_edit.description.clone());
        s_edit.form_price.set(format_balance(p_for_edit.price));
        s_edit.form_calls.set(p_for_edit.total_calls.to_string());
        s_edit.form_purchase_limit.set(
            p_for_edit
                .purchase_limit
                .map(|limit| limit.to_string())
                .unwrap_or_default(),
        );
        s_edit
            .form_validity
            .set(p_for_edit.validity_days.to_string());
        s_edit.form_status.set(p_for_edit.status.clone());
        s_edit.form_error.set(None);
        s_edit.editing_plan.set(Some(p_for_edit.clone()));
        s_edit.modal_kind.set(ModalKind::Edit);
    };
    let delete_handler = move |_: MouseEvent| {
        s_delete.form_error.set(None);
        s_delete.deleting_plan.set(Some(p_for_delete.clone()));
        s_delete.modal_kind.set(ModalKind::DeleteConfirm);
    };

    rsx! {
        tr { key: "{id}",
            td {
                span {
                    class: "ains-table__name-cell",
                    title: "{description}",
                    "data-id": "{id}",
                    "{name}"
                }
            }
            if actor_is_system {
                td { "{tenant_label}" }
            }
            td { class: "ains-table__mono ains-table__align--right", "{price_display}" }
            td { class: "ains-table__mono ains-table__align--right", "{plan.total_calls}" }
            td { class: "ains-table__mono ains-table__align--right", "{plan.validity_days}" }
            td { class: "ains-table__mono ains-table__align--right", "{format_purchase_limit(t, plan.purchase_limit)}" }
            td {
                if is_active {
                    Badge { variant: BadgeVariant::User, "{t.plans_badge_active}" }
                } else {
                    Badge { variant: BadgeVariant::Admin, "{t.plans_badge_disabled}" }
                }
            }
            td { class: "ains-table__mono", "{format_dt(&created)}" }
            td {
                div { class: "ains-table__row-actions",
                    button {
                        class: "ains-table__action",
                        title: "{t.plans_edit_title}",
                        onclick: edit_handler,
                        Pencil {}
                    }
                    button {
                        class: "ains-table__action ains-table__action--danger",
                        title: "{t.plans_delete_title}",
                        onclick: delete_handler,
                        Trash2 {}
                    }
                }
            }
        }
    }
}

fn close_all(mut signals: PlansSignals) {
    signals.modal_kind.set(ModalKind::None);
    signals.editing_plan.set(None);
    signals.deleting_plan.set(None);
    signals.form_tenant_id.set(String::new());
    signals.tenant_dropdown_open.set(false);
    signals.form_name.set(String::new());
    signals.form_desc.set(String::new());
    signals.form_price.set(String::new());
    signals.form_calls.set(String::new());
    signals.form_purchase_limit.set(String::new());
    signals.form_validity.set(String::new());
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
    editing: Option<PlanResponse>,
    deleting: Option<PlanResponse>,
    signals: PlansSignals,
    actor_is_system: bool,
    available_tenants: Signal<Vec<TenantResponse>>,
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
        t,
        kind,
        form_error,
        submitting,
        editing,
        signals,
        actor_is_system,
        available_tenants,
        client,
        log_bus,
        auth,
        nav,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_delete_confirm(
    t: &'static Translations,
    deleting: Option<PlanResponse>,
    submitting: bool,
    signals: PlansSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
) -> Element {
    let message = deleting
        .as_ref()
        .map(|plan| tf(t.plans_confirm_delete_msg, &[("name", &plan.name)]))
        .unwrap_or_else(|| t.plans_no_target.to_string());
    let confirm_delete_title = t.plans_confirm_delete_title.to_string();

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
        let Some(plan) = s_async.deleting_plan.cloned() else {
            return;
        };
        let target_id = plan.id.clone();
        s_async.submitting.set(true);
        let client_async = c_async.clone();
        let bus_async = b_async;
        let mut s_inner = s_async;
        let auth_async = a_async.clone();
        let lang = use_context::<I18nContext>().lang();
        spawn(async move {
            let res = client_async.delete_plan(&target_id).await;
            s_inner.submitting.set(false);
            match res {
                Ok(_) => {
                    push_log_ok(
                        bus_async,
                        HttpMethod::Delete,
                        &format!("/api/plans/{target_id}"),
                    );
                    if *s_inner.modal_kind.read() == ModalKind::DeleteConfirm {
                        s_inner.modal_kind.set(ModalKind::None);
                        s_inner.deleting_plan.set(None);
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
                        &format!("/api/plans/{target_id}"),
                        &err,
                    );
                    s_inner.form_error.set(Some(humanize_error(
                        &err,
                        ErrorContext::PlanManagement,
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
            confirm_label: t.plans_confirm_delete_btn.to_string(),
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
    editing: Option<PlanResponse>,
    signals: PlansSignals,
    actor_is_system: bool,
    available_tenants: Signal<Vec<TenantResponse>>,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
) -> Element {
    let title_str = if kind == ModalKind::Create {
        t.plans_modal_create_title
    } else {
        t.plans_modal_edit_title
    };
    let submit_label = if kind == ModalKind::Create {
        t.plans_modal_create_submit
    } else {
        t.plans_modal_edit_submit
    };

    let name_empty = t.plans_name_empty.to_string();
    let invalid_numbers = t.plans_invalid_numbers.to_string();
    let invalid_validity_range = t.plans_invalid_validity_range.to_string();
    let invalid_purchase_limit = t.plans_invalid_purchase_limit.to_string();
    let tenant_required = t.plans_tenant_required.to_string();
    let no_target_id = t.plans_modal_no_target_id.to_string();

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
    let on_submit = move |event: FormEvent| {
        event.prevent_default();
        if *signals_for_submit.submitting.read() {
            return;
        }
        let name = signals_for_submit.form_name.cloned();
        let description = signals_for_submit.form_desc.cloned();
        let tenant_id = signals_for_submit.form_tenant_id.cloned();
        let status = signals_for_submit.form_status.cloned();
        let editing_id = editing_for_submit.as_ref().map(|p| p.id.clone());
        let kind_now = kind;
        if name.trim().is_empty() {
            signals_for_submit.form_error.set(Some(name_empty.clone()));
            return;
        }
        // 数值字段统一校验：价格（展示单位）非负，次数与有效期必须为正。
        let price_text = signals_for_submit.form_price.cloned();
        let price = parse_display_amount(&price_text);
        let calls = signals_for_submit
            .form_calls
            .cloned()
            .trim()
            .parse::<i64>()
            .ok();
        let validity = signals_for_submit
            .form_validity
            .cloned()
            .trim()
            .parse::<i32>()
            .ok();
        let purchase_limit = parse_purchase_limit(&signals_for_submit.form_purchase_limit.cloned());
        let (Some(price), Some(calls), Some(validity)) = (price, calls, validity) else {
            signals_for_submit
                .form_error
                .set(Some(invalid_numbers.clone()));
            return;
        };
        if price < 0 || calls <= 0 {
            signals_for_submit
                .form_error
                .set(Some(invalid_numbers.clone()));
            return;
        }
        if !validity_days_in_range(validity) {
            signals_for_submit
                .form_error
                .set(Some(invalid_validity_range.clone()));
            return;
        }
        let Some(purchase_limit) = purchase_limit else {
            signals_for_submit
                .form_error
                .set(Some(invalid_purchase_limit.clone()));
            return;
        };
        if kind_now == ModalKind::Create && actor_is_system && tenant_id.is_empty() {
            signals_for_submit
                .form_error
                .set(Some(tenant_required.clone()));
            return;
        }
        // 编辑时价格输入框由 format_balance 预填（截断到 2 位小数）。
        // 仅当用户实际修改了该输入时才携带 price 字段，避免一次普通
        // 保存把高精度价格（存储单位 10^10 刻度允许更细）静默截断改写。
        let price_field = price_update_field(
            editing_for_submit.as_ref().map(|p| p.price),
            &price_text,
            price,
        );
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
                    let r = client_async
                        .create_plan(CreatePlanRequest {
                            tenant_id: actor_is_system.then_some(tenant_id),
                            name,
                            description: Some(description),
                            price,
                            total_calls: calls,
                            validity_days: validity,
                            purchase_limit,
                            status: None,
                        })
                        .await;
                    if r.is_ok() {
                        push_log_ok(bus_async, HttpMethod::Post, "/api/plans");
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
                        .update_plan(
                            id,
                            UpdatePlanRequest {
                                name: Some(name),
                                description: Some(description),
                                price: price_field,
                                total_calls: Some(calls),
                                validity_days: Some(validity),
                                purchase_limit: Some(purchase_limit),
                                status: Some(status),
                            },
                        )
                        .await;
                    if r.is_ok() {
                        push_log_ok(bus_async, HttpMethod::Put, &format!("/api/plans/{}", id));
                    }
                    r
                }
                ModalKind::DeleteConfirm | ModalKind::None => unreachable!(),
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
                        "/api/plans".to_string()
                    } else {
                        format!("/api/plans/{}", editing_id.unwrap_or_default())
                    };
                    push_log_err(bus_async, log_method, &log_path, &err);
                    s_async.form_error.set(Some(humanize_error(
                        &err,
                        ErrorContext::PlanManagement,
                        lang,
                    )));
                }
            }
        });
    };

    let status_now = signals.form_status.cloned();
    let is_create = kind == ModalKind::Create;

    // 自定义租户下拉已抽取为共享组件（views::tenant_select），
    // 与用户管理共用同一实现，避免两处副本在维护中漂移。

    rsx! {
        Modal {
            title: title_str.to_string(),
            on_close,
            open: true,
            disable_backdrop: submitting,
            disable_close: submitting,
            form {
                class: "ains-form-stack",
                onsubmit: on_submit,
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
                // system 创建套餐时需选择归属租户；编辑时租户不可变更。
                // 共享自绘下拉与用户管理完全对齐；字段位于 Modal 顶部，
                // 故用 drop_down 变体向下展开，避免弹层越出模态框顶部被裁切。
                if tenant_select_visible(kind, actor_is_system) {
                    {
                        render_tenant_select(TenantSelectView {
                            label: t.plans_form_tenant_label.to_string(),
                            panel_id: TENANT_PANEL_ID,
                            disabled: submitting,
                            drop_down: true,
                            // 套餐必须归属租户：未选时展示占位文案（提交时同样被校验拦截）。
                            empty_placeholder: t.plans_tenant_required.to_string(),
                            available_tenants,
                            selected_id: signals.form_tenant_id,
                            open: signals.tenant_dropdown_open,
                            next_page: signals.tenant_next_page,
                            has_more: signals.tenant_has_more,
                            loading: signals.tenant_loading,
                            client: client.clone(),
                            auth: auth.clone(),
                            nav,
                            log_bus,
                        })
                    }
                }
                TextInput {
                    label: t.plans_form_name_label.to_string(),
                    placeholder: Some(t.plans_form_name_placeholder.to_string()),
                    value: signals.form_name,
                    required: true,
                    disabled: submitting,
                    name: Some("name".to_string()),
                }
                TextInput {
                    label: t.plans_form_desc_label.to_string(),
                    value: signals.form_desc,
                    disabled: submitting,
                    name: Some("description".to_string()),
                }
                TextInput {
                    label: t.plans_form_price_label.to_string(),
                    value: signals.form_price,
                    input_type: InputType::Number,
                    min: Some(MIN_PLAN_PRICE.to_string()),
                    step: Some(PLAN_PRICE_STEP.to_string()),
                    required: true,
                    disabled: submitting,
                    name: Some("price".to_string()),
                }
                TextInput {
                    label: t.plans_form_calls_label.to_string(),
                    value: signals.form_calls,
                    input_type: InputType::Number,
                    min: Some("1".to_string()),
                    step: Some("1".to_string()),
                    required: true,
                    disabled: submitting,
                    name: Some("total_calls".to_string()),
                }
                TextInput {
                    label: t.plans_form_purchase_limit_label.to_string(),
                    value: signals.form_purchase_limit,
                    input_type: InputType::Number,
                    min: Some("1".to_string()),
                    step: Some("1".to_string()),
                    disabled: submitting,
                    name: Some("purchase_limit".to_string()),
                }
                TextInput {
                    label: t.plans_form_validity_label.to_string(),
                    value: signals.form_validity,
                    input_type: InputType::Number,
                    min: Some("1".to_string()),
                    max: Some("36500".to_string()),
                    step: Some("1".to_string()),
                    required: true,
                    disabled: submitting,
                    name: Some("validity_days".to_string()),
                }
                // Status toggle only in Edit mode (created plans default to active)
                if !is_create {
                    div { class: "ains-form-field",
                        span { id: "plan-status-label", class: "ains-form-label", "{t.plans_form_status_label}" }
                        div {
                            class: "ains-form-pill-group",
                            role: "group",
                            aria_labelledby: "plan-status-label",
                            button {
                                r#type: "button",
                                class: if status_now == "active" { "ains-form-pill ains-form-pill--active" } else { "ains-form-pill" },
                                aria_pressed: if status_now == "active" { "true" } else { "false" },
                                onclick: pick_active,
                                "{t.plans_form_status_active}"
                            }
                            button {
                                r#type: "button",
                                class: if status_now == "disabled" { "ains-form-pill ains-form-pill--active" } else { "ains-form-pill" },
                                aria_pressed: if status_now == "disabled" { "true" } else { "false" },
                                onclick: pick_disabled,
                                "{t.plans_form_status_disabled}"
                            }
                        }
                    }
                }
                Button {
                    button_type: ButtonType::Submit,
                    full_width: true,
                    disabled: submitting,
                    loading: submitting,
                    onclick: None,
                    "{submit_label}"
                }
            }
        }
    }
}

fn build_columns(t: &'static Translations, actor_is_system: bool) -> Vec<Column> {
    let mut columns = vec![Column::new(t.plans_column_name).align(Align::Left)];
    if actor_is_system {
        columns.push(Column::new(t.plans_column_tenant).align(Align::Left));
    }
    columns.extend([
        Column::new(t.plans_column_price)
            .width("w-24")
            .align(Align::Right),
        Column::new(t.plans_column_calls)
            .width("w-20")
            .align(Align::Right),
        Column::new(t.plans_column_validity)
            .width("w-20")
            .align(Align::Right),
        Column::new(t.plans_column_purchase_limit)
            .width("w-24")
            .align(Align::Right),
        Column::new(t.plans_column_status)
            .width("w-24")
            .align(Align::Center),
        Column::new(t.plans_column_created)
            .width("w-40")
            .align(Align::Left),
        Column::new(t.plans_column_actions)
            .width("w-32")
            .align(Align::Center),
    ]);
    columns
}

fn format_dt(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M").to_string()
}

fn validity_days_in_range(validity_days: i32) -> bool {
    (1..=MAX_PLAN_VALIDITY_DAYS).contains(&validity_days)
}

/// Blank means unlimited; non-blank values must be positive integers.
fn parse_purchase_limit(value: &str) -> Option<Option<i32>> {
    let value = value.trim();
    if value.is_empty() {
        return Some(None);
    }
    value
        .parse::<i32>()
        .ok()
        .filter(|limit| *limit > 0)
        .map(Some)
}

/// UI 纯函数：仅「创建模式 + system 角色」展示所属租户下拉
/// （编辑时租户不可变更；admin 由服务端强制限定自身租户）。
fn tenant_select_visible(kind: ModalKind, actor_is_system: bool) -> bool {
    kind == ModalKind::Create && actor_is_system
}

/// 决定编辑提交时是否携带 price 字段。
///
/// 输入框由 `format_balance` 预填（截断到 2 位小数）：若用户未改动
/// 该输入（文本仍等于预填值）则返回 `None`，避免一次普通保存把
/// 高精度价格（存储单位 10^10 刻度允许更细）静默截断改写。
/// 新建场景（`editing_price == None`）总是携带解析后的价格。
fn price_update_field(
    editing_price: Option<i64>,
    price_text: &str,
    parsed_price: i64,
) -> Option<i64> {
    match editing_price {
        Some(stored) if format_balance(stored) == price_text => None,
        _ => Some(parsed_price),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MIN_PLAN_PRICE, ModalKind, PLAN_PRICE_STEP, parse_purchase_limit, price_update_field,
        tenant_select_visible, validity_days_in_range,
    };
    use crate::balance::{BALANCE_SCALE, format_balance, parse_display_amount};
    use client_api::UpdatePlanRequest;

    #[test]
    fn tenant_select_only_for_system_create() {
        // 仅 system 创建时可见；非 system 或非创建模式均不渲染 ains-select。
        assert!(tenant_select_visible(ModalKind::Create, true));
        assert!(!tenant_select_visible(ModalKind::Create, false));
        assert!(!tenant_select_visible(ModalKind::Edit, true));
        assert!(!tenant_select_visible(ModalKind::Edit, false));
        assert!(!tenant_select_visible(ModalKind::DeleteConfirm, true));
        assert!(!tenant_select_visible(ModalKind::None, true));
    }

    #[test]
    fn purchase_limit_parser_supports_unlimited_and_positive_values() {
        assert_eq!(parse_purchase_limit(""), Some(None));
        assert_eq!(parse_purchase_limit("  "), Some(None));
        assert_eq!(parse_purchase_limit("1"), Some(Some(1)));
        assert_eq!(parse_purchase_limit("2147483647"), Some(Some(i32::MAX)));
        assert_eq!(parse_purchase_limit("0"), None);
        assert_eq!(parse_purchase_limit("-1"), None);
        assert_eq!(parse_purchase_limit("1.5"), None);
        assert_eq!(parse_purchase_limit("2147483648"), None);
    }

    #[test]
    fn edit_purchase_limit_maps_blank_to_explicit_json_null() {
        let unlimited = parse_purchase_limit("").expect("blank means unlimited");
        let request = UpdatePlanRequest {
            purchase_limit: Some(unlimited),
            ..Default::default()
        };

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({ "purchase_limit": null })
        );

        let limited = parse_purchase_limit("3").expect("positive limit");
        let request = UpdatePlanRequest {
            purchase_limit: Some(limited),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({ "purchase_limit": 3 })
        );
    }

    #[test]
    fn validity_days_enforces_the_same_documented_bounds() {
        assert!(!validity_days_in_range(0));
        assert!(validity_days_in_range(1));
        assert!(validity_days_in_range(36_500));
        assert!(!validity_days_in_range(36_501));
    }

    #[test]
    fn create_always_sends_price() {
        assert_eq!(price_update_field(None, "10.50", 42), Some(42));
    }

    #[test]
    fn price_input_constraints_preserve_the_exact_decimal_contract() {
        assert_eq!(parse_display_amount(MIN_PLAN_PRICE), Some(0));
        assert_eq!(PLAN_PRICE_STEP, "any");
        assert_eq!(
            parse_display_amount("10.0000000001"),
            Some(BALANCE_SCALE * 10 + 1)
        );
    }

    #[test]
    fn untouched_prefill_is_omitted() {
        // 高精度价格：10.5 + 1 个最小存储单位，预填文本截断为 "10.50"。
        let stored = BALANCE_SCALE * 10 + BALANCE_SCALE / 2 + 1;
        let prefill = format_balance(stored);
        let parsed = parse_display_amount(&prefill).unwrap();
        // 未改动输入 → 不携带 price，高精度值不被截断改写。
        assert_eq!(price_update_field(Some(stored), &prefill, parsed), None);
    }

    #[test]
    fn edited_price_is_sent() {
        let stored = BALANCE_SCALE * 10;
        let parsed = parse_display_amount("12.25").unwrap();
        assert_eq!(
            price_update_field(Some(stored), "12.25", parsed),
            Some(parsed)
        );
    }

    #[test]
    fn same_value_retyped_differently_is_sent_verbatim() {
        // "10.5" 与预填 "10.50" 文本不等 → 视为用户改动，携带解析值。
        let stored = BALANCE_SCALE * 10 + BALANCE_SCALE / 2;
        let parsed = parse_display_amount("10.5").unwrap();
        assert_eq!(
            price_update_field(Some(stored), "10.5", parsed),
            Some(parsed)
        );
        // 解析值与存储值一致，发送也无副作用。
        assert_eq!(parsed, stored);
    }
}
