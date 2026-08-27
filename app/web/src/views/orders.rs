//! 支付订单视图（admin/system）。
//!
//! 参照 `plans.rs` / `tenants.rs` 的架构模式，接入 `client-api` 的
//! list / create / update / delete。订单为租户隔离资源：system 可见全部
//! 租户（含租户列），admin 由服务端按订单的 tenant_id 快照过滤。
//!
//! 注意：订单状态变更仅为记录性操作 —— 改为 refunded/cancelled 不会
//! 回补余额、也不会撤销已分配的套餐（与服务端语义一致）。

use std::cell::Cell;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use client_api::{CreateOrderRequest, PaymentOrderResponse, UpdateOrderRequest};
use dioxus::prelude::dioxus_router::Navigator;
use dioxus::prelude::*;
use dioxus_icons::lucide::{LoaderCircle, Pencil, Plus, Trash2, TriangleAlert};

use ui::{
    Align, Badge, BadgeVariant, Button, ButtonType, Column, DataTable, I18nContext, Modal,
    TextInput, Translations, tf,
};

use crate::api::{ErrorContext, humanize_error};
use crate::auth::AuthState;
use crate::balance::{format_balance, parse_display_amount};
use crate::components::{
    ConfirmDialog, HttpMethod, LogBus, SearchSignal, push_log_err, push_log_ok,
};
use crate::views::{
    PaginationEntity, format_pagination_info, order_method_label as method_label,
    order_status_label as status_label,
};

const ORDER_STATUSES: &[&str] = &["paid", "pending", "refunded", "cancelled"];
const PAYMENT_METHODS: &[&str] = &["balance", "wechat", "alipay"];

#[derive(Debug, Clone)]
enum ListState {
    Loading,
    Loaded(Vec<PaymentOrderResponse>),
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
struct OrdersSignals {
    modal_kind: Signal<ModalKind>,
    editing_order: Signal<Option<PaymentOrderResponse>>,
    deleting_order: Signal<Option<PaymentOrderResponse>>,
    form_user_id: Signal<String>,
    form_amount: Signal<String>,
    form_status: Signal<String>,
    form_method: Signal<String>,
    form_txn: Signal<String>,
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
pub fn Orders() -> Element {
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

    let signals = OrdersSignals {
        modal_kind: use_signal(|| ModalKind::None),
        editing_order: use_signal(|| Option::<PaymentOrderResponse>::None),
        deleting_order: use_signal(|| Option::<PaymentOrderResponse>::None),
        form_user_id: use_signal(String::new),
        form_amount: use_signal(String::new),
        form_status: use_signal(|| "paid".to_string()),
        form_method: use_signal(|| "balance".to_string()),
        form_txn: use_signal(String::new),
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
                    .list_orders(current_page, current_per_page, None)
                    .await;
                if version_check() != version
                    || page() != current_page
                    || per_page() != current_per_page
                {
                    return;
                }
                match res {
                    Ok(page_data) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/orders");
                        total_signal.set(page_data.total);
                        total_pages_signal.set(page_data.total_pages);
                        list.set(ListState::Loaded(page_data.items));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/orders", &err);
                        list.set(ListState::Error(humanize_error(
                            &err,
                            ErrorContext::OrderManagement,
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
    let editing_snapshot = signals.editing_order.read().clone();
    let deleting_snapshot = signals.deleting_order.read().clone();

    let page_val = page();
    let per_page_val = per_page();
    let total_val = total();
    let total_pages_val = total_pages();

    let mut signals_for_open = signals;
    let open_create = move |_: MouseEvent| {
        signals_for_open.form_user_id.set(String::new());
        signals_for_open.form_amount.set(String::new());
        signals_for_open.form_status.set("paid".to_string());
        signals_for_open.form_method.set("balance".to_string());
        signals_for_open.form_txn.set(String::new());
        signals_for_open.form_error.set(None);
        signals_for_open.editing_order.set(None);
        signals_for_open.modal_kind.set(ModalKind::Create);
    };

    rsx! {
        div { class: "ains-users",
            header { class: "ains-users__header",
                div { class: "ains-users__title-block",
                    h1 { class: "ains-users__title", "{t.orders_title}" }
                    p { class: "ains-users__subtitle", "{t.orders_subtitle}" }
                }
                div { class: "ains-users__header-actions",
                    Button { onclick: open_create,
                        Plus {}
                        "{t.orders_create_btn}"
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
    signals: OrdersSignals,
    actor_is_system: bool,
    pagination: PaginationState,
) -> Element {
    match list_snapshot {
        ListState::Loading => rsx! {
            div { class: "ains-users__status",
                LoaderCircle { class: "ains-btn__spinner" }
                "{t.orders_loading}"
            }
        },
        ListState::Error(msg) => rsx! {
            div { class: "ains-users__status ains-users__status--error",
                TriangleAlert {}
                "{msg}"
            }
        },
        ListState::Loaded(items) => {
            let filtered: Vec<PaymentOrderResponse> = if search_text.is_empty() {
                items
            } else {
                let q = search_text.to_lowercase();
                items
                    .into_iter()
                    .filter(|o| {
                        o.user_email.to_lowercase().contains(&q)
                            || o.plan_name.to_lowercase().contains(&q)
                            || o.id.contains(&q)
                    })
                    .collect()
            };
            let columns = build_columns(t, actor_is_system);
            let rows: Vec<Element> = filtered
                .into_iter()
                .map(|order| row_element(t, order, signals, actor_is_system))
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
                format_pagination_info(t, PaginationEntity::Orders, total, page, total_pages);

            rsx! {
                div { class: "ains-users__table-wrapper",
                    DataTable {
                        columns,
                        rows,
                        empty: Some(rsx! { "{t.orders_empty}" }),
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
    order: PaymentOrderResponse,
    signals: OrdersSignals,
    actor_is_system: bool,
) -> Element {
    let id = order.id.clone();
    let user_email = order.user_email.clone();
    let plan_name = if order.plan_name.is_empty() {
        "-".to_string()
    } else {
        order.plan_name.clone()
    };
    let tenant_label = order
        .tenant_name
        .clone()
        .unwrap_or_else(|| order.tenant_id.clone());
    let amount_display = format_balance(order.amount);
    let status = order.status.clone();
    let method = order.payment_method.clone();
    let created = order.created_at;
    let is_paid = status == "paid";

    let o_for_edit = order.clone();
    let o_for_delete = order.clone();
    let mut s_edit = signals;
    let mut s_delete = signals;
    let edit_handler = move |_: MouseEvent| {
        s_edit.form_status.set(o_for_edit.status.clone());
        s_edit.form_method.set(o_for_edit.payment_method.clone());
        s_edit
            .form_txn
            .set(o_for_edit.external_txn_id.clone().unwrap_or_default());
        s_edit.form_error.set(None);
        s_edit.editing_order.set(Some(o_for_edit.clone()));
        s_edit.modal_kind.set(ModalKind::Edit);
    };
    let delete_handler = move |_: MouseEvent| {
        s_delete.form_error.set(None);
        s_delete.deleting_order.set(Some(o_for_delete.clone()));
        s_delete.modal_kind.set(ModalKind::DeleteConfirm);
    };

    rsx! {
        tr { key: "{id}",
            td { class: "ains-table__mono", title: "{id}", "{id}" }
            td {
                span { class: "ains-table__name-cell", "{user_email}" }
            }
            if actor_is_system {
                td { "{tenant_label}" }
            }
            td { "{plan_name}" }
            td { class: "ains-table__mono ains-table__align--right", "{amount_display}" }
            td { "{method_label(t, &method)}" }
            td {
                if is_paid {
                    Badge { variant: BadgeVariant::User, "{status_label(t, &status)}" }
                } else {
                    Badge { variant: BadgeVariant::Admin, "{status_label(t, &status)}" }
                }
            }
            td { class: "ains-table__mono", "{format_dt(&created)}" }
            td {
                div { class: "ains-table__row-actions",
                    button {
                        class: "ains-table__action",
                        title: "{t.orders_edit_title}",
                        onclick: edit_handler,
                        Pencil {}
                    }
                    // 订单是资金流水审计记录，服务端仅允许 system 角色删除；
                    // 非 system 角色隐藏入口，避免点击后收到 403。
                    if actor_is_system {
                        button {
                            class: "ains-table__action ains-table__action--danger",
                            title: "{t.orders_delete_title}",
                            onclick: delete_handler,
                            Trash2 {}
                        }
                    }
                }
            }
        }
    }
}

fn close_all(mut signals: OrdersSignals) {
    signals.modal_kind.set(ModalKind::None);
    signals.editing_order.set(None);
    signals.deleting_order.set(None);
    signals.form_user_id.set(String::new());
    signals.form_amount.set(String::new());
    signals.form_status.set("paid".to_string());
    signals.form_method.set("balance".to_string());
    signals.form_txn.set(String::new());
    signals.submitting.set(false);
    signals.form_error.set(None);
}

#[allow(clippy::too_many_arguments)]
fn render_modal(
    t: &'static Translations,
    kind: ModalKind,
    form_error: Option<String>,
    submitting: bool,
    editing: Option<PaymentOrderResponse>,
    deleting: Option<PaymentOrderResponse>,
    signals: OrdersSignals,
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
    deleting: Option<PaymentOrderResponse>,
    submitting: bool,
    signals: OrdersSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
) -> Element {
    let message = deleting
        .as_ref()
        .map(|order| tf(t.orders_confirm_delete_msg, &[("id", &order.id)]))
        .unwrap_or_else(|| t.orders_no_target.to_string());
    let confirm_delete_title = t.orders_confirm_delete_title.to_string();

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
        let Some(order) = s_async.deleting_order.cloned() else {
            return;
        };
        let target_id = order.id.clone();
        s_async.submitting.set(true);
        let client_async = c_async.clone();
        let bus_async = b_async;
        let mut s_inner = s_async;
        let auth_async = a_async.clone();
        let lang = use_context::<I18nContext>().lang();
        spawn(async move {
            let res = client_async.delete_order(&target_id).await;
            s_inner.submitting.set(false);
            match res {
                Ok(_) => {
                    push_log_ok(
                        bus_async,
                        HttpMethod::Delete,
                        &format!("/api/orders/{target_id}"),
                    );
                    if *s_inner.modal_kind.read() == ModalKind::DeleteConfirm {
                        s_inner.modal_kind.set(ModalKind::None);
                        s_inner.deleting_order.set(None);
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
                        &format!("/api/orders/{target_id}"),
                        &err,
                    );
                    s_inner.form_error.set(Some(humanize_error(
                        &err,
                        ErrorContext::OrderManagement,
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
            confirm_label: t.orders_confirm_delete_btn.to_string(),
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
    editing: Option<PaymentOrderResponse>,
    signals: OrdersSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: Navigator,
) -> Element {
    let title_str = if kind == ModalKind::Create {
        t.orders_modal_create_title
    } else {
        t.orders_modal_edit_title
    };
    let submit_label = if kind == ModalKind::Create {
        t.orders_modal_create_submit
    } else {
        t.orders_modal_edit_submit
    };

    let invalid_user_id = t.orders_invalid_user_id.to_string();
    let invalid_amount = t.orders_invalid_amount.to_string();
    let no_target_id = t.orders_modal_no_target_id.to_string();

    let signals_for_close = signals;
    let on_close = move |_: MouseEvent| {
        if !*signals_for_close.submitting.read() {
            close_all(signals_for_close);
        }
    };

    let editing_for_submit = editing.clone();
    let mut signals_for_submit = signals;
    let auth_for_submit = auth.clone();
    let client_for_submit = client.clone();
    let on_submit = move |_: MouseEvent| {
        if *signals_for_submit.submitting.read() {
            return;
        }
        let user_id = signals_for_submit.form_user_id.cloned();
        let status = signals_for_submit.form_status.cloned();
        let method = signals_for_submit.form_method.cloned();
        let txn = signals_for_submit.form_txn.cloned();
        let editing_id = editing_for_submit.as_ref().map(|o| o.id.clone());
        let kind_now = kind;
        if kind_now == ModalKind::Create {
            if user_id.trim().parse::<i64>().is_err() {
                signals_for_submit
                    .form_error
                    .set(Some(invalid_user_id.clone()));
                return;
            }
            if parse_display_amount(&signals_for_submit.form_amount.cloned()).is_none() {
                signals_for_submit
                    .form_error
                    .set(Some(invalid_amount.clone()));
                return;
            }
        }
        let amount = parse_display_amount(&signals_for_submit.form_amount.cloned());
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
                        .create_order(CreateOrderRequest {
                            user_id: user_id.trim().to_string(),
                            plan_id: None,
                            amount: amount.unwrap_or(0),
                            status: Some(status),
                            payment_method: Some(method),
                            external_txn_id: (!txn.trim().is_empty())
                                .then(|| txn.trim().to_string()),
                        })
                        .await;
                    if r.is_ok() {
                        push_log_ok(bus_async, HttpMethod::Post, "/api/orders");
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
                        .update_order(
                            id,
                            UpdateOrderRequest {
                                status: Some(status),
                                payment_method: Some(method),
                                // 传空字符串即清除外部交易号（服务端将其置为 NULL）。
                                external_txn_id: Some(txn.trim().to_string()),
                            },
                        )
                        .await;
                    if r.is_ok() {
                        push_log_ok(bus_async, HttpMethod::Put, &format!("/api/orders/{}", id));
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
                        "/api/orders".to_string()
                    } else {
                        format!("/api/orders/{}", editing_id.unwrap_or_default())
                    };
                    push_log_err(bus_async, log_method, &log_path, &err);
                    s_async.form_error.set(Some(humanize_error(
                        &err,
                        ErrorContext::OrderManagement,
                        lang,
                    )));
                }
            }
        });
    };

    let status_now = signals.form_status.cloned();
    let method_now = signals.form_method.cloned();
    let is_create = kind == ModalKind::Create;
    let mut signals_for_status = signals;
    let mut signals_for_method = signals;

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
                if is_create {
                    TextInput {
                        label: t.orders_form_user_id_label.to_string(),
                        placeholder: Some(t.orders_form_user_id_placeholder.to_string()),
                        value: signals.form_user_id,
                        required: true,
                        disabled: submitting,
                        name: Some("user_id".to_string()),
                    }
                    TextInput {
                        label: t.orders_form_amount_label.to_string(),
                        value: signals.form_amount,
                        required: true,
                        disabled: submitting,
                        name: Some("amount".to_string()),
                    }
                }
                div { class: "ains-form-field",
                    label { class: "ains-form-label", "{t.orders_form_status_label}" }
                    div { class: "ains-form-pill-group",
                        for status in ORDER_STATUSES {
                            button {
                                r#type: "button",
                                class: if status_now == *status { "ains-form-pill ains-form-pill--active" } else { "ains-form-pill" },
                                onclick: move |_| signals_for_status.form_status.set(status.to_string()),
                                "{status_label(t, status)}"
                            }
                        }
                    }
                }
                div { class: "ains-form-field",
                    label { class: "ains-form-label", "{t.orders_form_method_label}" }
                    div { class: "ains-form-pill-group",
                        for method in PAYMENT_METHODS {
                            button {
                                r#type: "button",
                                class: if method_now == *method { "ains-form-pill ains-form-pill--active" } else { "ains-form-pill" },
                                onclick: move |_| signals_for_method.form_method.set(method.to_string()),
                                "{method_label(t, method)}"
                            }
                        }
                    }
                }
                TextInput {
                    label: t.orders_form_txn_label.to_string(),
                    value: signals.form_txn,
                    disabled: submitting,
                    name: Some("external_txn_id".to_string()),
                }
                // 状态变更仅记录，不产生资金/套餐副作用（与服务端语义一致）。
                if !is_create {
                    p { class: "ains-users__subtitle", "{t.orders_update_note}" }
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

fn build_columns(t: &'static Translations, actor_is_system: bool) -> Vec<Column> {
    let mut columns = vec![
        Column::new(t.orders_column_id)
            .width("w-40")
            .align(Align::Left),
        Column::new(t.orders_column_user).align(Align::Left),
    ];
    if actor_is_system {
        columns.push(Column::new(t.orders_column_tenant).align(Align::Left));
    }
    columns.extend([
        Column::new(t.orders_column_plan).align(Align::Left),
        Column::new(t.orders_column_amount)
            .width("w-24")
            .align(Align::Right),
        Column::new(t.orders_column_method)
            .width("w-24")
            .align(Align::Center),
        Column::new(t.orders_column_status)
            .width("w-24")
            .align(Align::Center),
        Column::new(t.orders_column_created)
            .width("w-40")
            .align(Align::Left),
        Column::new(t.orders_column_actions)
            .width("w-32")
            .align(Align::Center),
    ]);
    columns
}

fn format_dt(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M").to_string()
}
