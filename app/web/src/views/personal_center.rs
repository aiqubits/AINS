//! 个人中心视图（所有角色可访问）。
//!
//! 四区块：账户余额 / 我的套餐 / 可购套餐 / 账单记录，接入 client-api 的
//! get_me / list_my_plans / list_available_plans / purchase_plan /
//! list_my_orders。在线自助充值待支付网关接入（二期），余额卡片仅展示
//! 并提示联系管理员。
//!
//! 购买链路：确认弹窗 → purchase_plan → 成功后用返回的 balance（i64
//! 存储单位）直接更新余额信号（不重拉 get_me），并递增 refresh_version
//! 刷新"我的套餐"与"账单记录"；可购套餐列表不受影响（模板未变）。
//! 错误经 `ErrorContext::PersonalCenter` 翻译 —— 自助接口的 403 来自
//! 租户禁用而非权限不足，文案与管理页语境区分；因此错误拦截使用
//! `handle_unauth_401_only`（仅 401 登出跳转），403 落入业务文案渲染。
//!
//! 409 purchase_in_progress 保持弹窗打开并内联展示错误，由用户显式
//! 再次确认 —— **不自动重试**：服务端购买锁是防快速重复提交保护，
//! 自动重试会把跨页签被抑制的重复提交变成真实双扣费（顺序重复
//! 购买是有意叠加语义，见服务端 test_sequential_repurchase_stacks_instances）。

use chrono::{DateTime, Utc};
use client_api::{ClientError, PaymentOrderResponse, PlanResponse, UserPlanResponse};
use dioxus::prelude::*;
use dioxus_icons::lucide::{LoaderCircle, TriangleAlert, Wallet};

use ui::{Align, Badge, BadgeVariant, Column, DataTable, I18nContext, Translations, tf};

use crate::api::{ErrorContext, humanize_error};
use crate::auth::AuthState;
use crate::balance::format_balance;
use crate::components::{ConfirmDialog, HttpMethod, LogBus, push_log_err, push_log_ok};
use crate::views::{order_method_label, order_status_label};

/// 区块加载状态（余额单独用 `Option<i64>` + 错误信号表达）。
#[derive(Debug, Clone)]
enum SectionState<T> {
    Loading,
    Loaded(Vec<T>),
    Error(String),
}

/// 购买按钮可用性判断：存储单位 i64 精确比较。
///
/// 禁止经 f64（display 值）比较 —— f64 仅在 2^53 存储单位内精确，
/// 超出后会静默丢精度。服务端余额检查仍是最终兜底。
fn can_afford(balance: i64, price: i64) -> bool {
    balance >= price
}

/// 购买按钮是否可用：余额未加载（None）时禁用，避免盲买。
/// 与 [`show_insufficient_label`] 分离：余额未知 ≠ 余额不足，
/// 前者仍显示“购买”文案（置灰），后者显示“余额不足”。
fn buy_button_enabled(balance: Option<i64>, price: i64) -> bool {
    balance.is_some_and(|b| can_afford(b, price))
}

/// 是否展示“余额不足”文案：仅在余额已知且确实不足时。
fn show_insufficient_label(balance: Option<i64>, price: i64) -> bool {
    balance.is_some() && !buy_button_enabled(balance, price)
}

/// 套餐实例派生状态 → 本地化文案（服务端返回 active/expired/exhausted）。
/// 未知状态原样透出（中性回退），与徽章侧的灰色回退保持一致，
/// 避免出现“生效中文案 + 灰色徽章”的矛盾组合。
fn plan_status_label<'a>(t: &'static Translations, status: &'a str) -> &'a str {
    match status {
        "active" => t.pc_plan_status_active,
        "expired" => t.pc_plan_status_expired,
        "exhausted" => t.pc_plan_status_exhausted,
        other => other,
    }
}

/// 套餐实例状态 → 徽章颜色：生效中=绿 / 已用尽=橙 / 其余（含未知）=灰。
fn plan_status_variant(status: &str) -> BadgeVariant {
    match status {
        "active" => BadgeVariant::Success,
        "exhausted" => BadgeVariant::Warning,
        _ => BadgeVariant::User,
    }
}

/// 套餐实例来源 → 本地化文案（purchase / admin_grant，未知原样透出）。
fn plan_source_label<'a>(t: &'static Translations, source: &'a str) -> &'a str {
    match source {
        "purchase" => t.pc_plan_source_purchase,
        "admin_grant" => t.pc_plan_source_admin_grant,
        other => other,
    }
}

fn format_dt(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// 上一页页码（下界钳制到 1）。
fn prev_page(page: u64) -> u64 {
    page.saturating_sub(1).max(1)
}

/// 下一页页码（上界钳制到 total_pages，且永不低于 1 ——
/// 即使 total_pages == 0 时被调用也不会产生非法的第 0 页）。
/// saturating_add 使函数在全 u64 域上无 panic（全域契约，实际页码
/// 被服务端钳制在 1..=1_000_000 内）。
fn next_page(page: u64, total_pages: u64) -> u64 {
    page.saturating_add(1).min(total_pages).max(1)
}

/// get_me 响应过期判定：请求发起后手动重试（balance_version）或购买
/// 直更（balance_epoch）任一发生，该响应即过期，必须丢弃 —— 否则
/// 语言切换等触发的在途 get_me 会用购买前的陈旧余额覆盖购买接口
/// 返回的权威余额，短暂误启用买不起的购买按钮（服务端 400 仍兜底）。
fn balance_response_is_stale(
    seen_version: u64,
    seen_epoch: u64,
    current_version: u64,
    current_epoch: u64,
) -> bool {
    seen_version != current_version || seen_epoch != current_epoch
}

/// 购买成功横幅的存活时长（毫秒）：到期自动清除，避免陈旧的
/// “购买成功”与后续操作的状态长期并存造成误导。
const SUCCESS_BANNER_TTL_MS: u32 = 5_000;

/// 购买失败后是否保持确认弹窗打开：仅 409 purchase_in_progress。
///
/// 该错误是瞬态的“稍后重试”而非终态拒绝 —— 关闭弹窗会迫使用户
/// 重新发起整个购买流程；保持打开并内联展示错误，用户可直接再次
/// 确认。其余错误（余额不足/套餐下架/租户禁用等）均为终态，关闭
/// 弹窗并展示区块错误横幅。
fn purchase_error_keeps_dialog_open(err: &ClientError) -> bool {
    match err {
        ClientError::Other(409, body) => serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|c| c.as_str())
                    .map(|c| c == "purchase_in_progress")
            })
            .unwrap_or(false),
        _ => false,
    }
}

/// 购买链路共享信号（Copy，便于跨渲染函数与闭包传递）。
#[derive(Clone, Copy)]
struct PurchaseSignals {
    buying_plan: Signal<Option<PlanResponse>>,
    submitting: Signal<bool>,
    feedback_ok: Signal<bool>,
    feedback_err: Signal<Option<String>>,
    /// 确认弹窗内联错误（仅 409 purchase_in_progress 使用，见
    /// [`purchase_error_keeps_dialog_open`]）；与 feedback_err 互斥 ——
    /// 弹窗内错误不得同时出现在被遮罩盖住的区块横幅里。
    dialog_err: Signal<Option<String>>,
    balance: Signal<Option<i64>>,
    /// 购买直更余额时递增：作废在途 get_me（见 [`balance_response_is_stale`]）。
    balance_epoch: Signal<u64>,
    /// 账单分页页码：购买成功后重置到第 1 页，保证新订单（最新
    /// 在前）紧随“购买成功”提示可见，不被停留在后面页码遮蔽。
    orders_page: Signal<u64>,
    refresh_version: Signal<u64>,
    /// 成功横幅代际：每次成功递增；TTL 定时清除前复核，防止
    /// 旧定时器误清更新一次购买的横幅。
    feedback_seq: Signal<u64>,
}

/// 购买成功后的全部信号变更（集中于此以便单测编排）：
/// 1. 余额直取服务端返回值（i64 存储单位），不重拉 get_me；
/// 2. 递增 balance_epoch 作废所有购买前发起的在途 get_me；
/// 3. 关闭弹窗、清除错误、点亮成功横幅（递增 feedback_seq）；
/// 4. 账单回第 1 页 + 递增 refresh_version 重拉套餐与账单。
fn apply_purchase_success(mut s: PurchaseSignals, new_balance: i64) {
    s.balance.set(Some(new_balance));
    s.balance_epoch.with_mut(|v| *v += 1);
    s.buying_plan.set(None);
    s.dialog_err.set(None);
    s.feedback_err.set(None);
    s.feedback_ok.set(true);
    s.feedback_seq.with_mut(|v| *v += 1);
    s.orders_page.set(1);
    s.refresh_version.with_mut(|v| *v += 1);
}

/// 购买失败后的信号变更：可重试错误（409）保持弹窗打开并内联
/// 展示；终态错误关闭弹窗，错误落到区块横幅。两路径互斥清理
/// 对方错误信号，避免双处同时展示。
fn apply_purchase_failure(mut s: PurchaseSignals, msg: String, keep_dialog_open: bool) {
    s.feedback_ok.set(false);
    if keep_dialog_open {
        s.feedback_err.set(None);
        s.dialog_err.set(Some(msg));
    } else {
        s.buying_plan.set(None);
        s.dialog_err.set(None);
        s.feedback_err.set(Some(msg));
    }
}

/// 账单分页状态（快照值 + 信号），与 plans/orders 管理页对齐。
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
pub fn PersonalCenter() -> Element {
    let auth = use_context::<AuthState>();
    let log_bus = use_context::<LogBus>();
    let nav = use_navigator();
    let locale = use_context::<I18nContext>();
    let t = locale.t();

    // 余额：None = 加载中；购买成功后由返回值直接覆盖。
    let balance = use_signal(|| Option::<i64>::None);
    let balance_error = use_signal(|| Option::<String>::None);
    let my_plans = use_signal(|| SectionState::<UserPlanResponse>::Loading);
    let available = use_signal(|| SectionState::<PlanResponse>::Loading);
    let orders = use_signal(|| SectionState::<PaymentOrderResponse>::Loading);
    // 购买成功后递增：仅驱动"我的套餐"与"账单记录"重拉。
    let refresh_version = use_signal(|| 0u64);
    // 余额加载失败后的手动重试入口（购买成功路径不经过它 ——
    // 余额直接取购买接口返回值，不重拉 get_me）。
    let balance_version = use_signal(|| 0u64);
    // 购买成功直更余额时递增：作废在途 get_me，防陈旧余额回写。
    let balance_epoch = use_signal(|| 0u64);
    // 可购套餐加载失败后的手动重试入口（购买是本视图核心动线，
    // 初次拉取失败不能只剩死路，否则只能整页刷新恢复）。
    let available_version = use_signal(|| 0u64);
    let page = use_signal(|| 1u64);
    let per_page = use_signal(|| 20u64);
    let total = use_signal(|| 0u64);
    let total_pages = use_signal(|| 0u64);

    let purchase = PurchaseSignals {
        buying_plan: use_signal(|| Option::<PlanResponse>::None),
        submitting: use_signal(|| false),
        feedback_ok: use_signal(|| false),
        feedback_err: use_signal(|| Option::<String>::None),
        dialog_err: use_signal(|| Option::<String>::None),
        balance,
        balance_epoch,
        orders_page: page,
        refresh_version,
        feedback_seq: use_signal(|| 0u64),
    };

    // 余额：初始拉取 + 错误重试（balance_version）；购买成功走返回值
    // 直更，不经过本 effect。同步读取 locale.lang() 会顺带订阅语言
    // 信号（切换语言时重拉一次，与姊妹视图行为一致）。
    // 带过期响应丢弃（balance_version / balance_epoch 复核），与
    // 我的套餐、账单 effect 的守卫模式对齐。
    {
        let client = auth.client.clone();
        let bus = log_bus;
        let auth_for_effect = auth.clone();
        use_effect(move || {
            let version = balance_version();
            let client = client.clone();
            let mut balance = balance;
            let mut balance_error = balance_error;
            let bus = bus;
            let auth_inner = auth_for_effect.clone();
            let effect_lang = locale.lang();
            spawn(async move {
                // epoch 在异步体内读取（不产生订阅）：购买直更递增它时
                // 不得重触发本 effect，否则会多发一次冗余 get_me。
                let epoch = balance_epoch();
                let res = client.get_me().await;
                if balance_response_is_stale(version, epoch, balance_version(), balance_epoch())
                    || locale.lang() != effect_lang
                {
                    return;
                }
                match res {
                    Ok(me) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/users/me");
                        // 清除残留错误横幅：语言切换等自动重拉失败后
                        // 再次成功时，不得让旧错误与新余额并存（手动
                        // 重试路径由 retry_handler 清除，不经过这里）。
                        balance_error.set(None);
                        balance.set(Some(me.balance));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth_401_only(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/users/me", &err);
                        balance_error.set(Some(humanize_error(
                            &err,
                            ErrorContext::PersonalCenter,
                            effect_lang,
                        )));
                    }
                }
            });
        });
    }

    // 我的套餐：初始 + 购买成功后（refresh_version）重拉；
    // 过期守卫复核 version 与 lang（语言切换重拉不改 version，
    // 旧语言的在途响应若后到会写入错语言的错误文案）。
    {
        let client = auth.client.clone();
        let bus = log_bus;
        let auth_for_effect = auth.clone();
        use_effect(move || {
            // version 在同步段读取（订阅 + 快照一步完成）：若在任务
            // 排队期间 refresh_version 再次递增，本任务会被下方复核
            // 判为过期而丢弃，不会与新 effect 触发的任务重复请求。
            let version = refresh_version();
            let client = client.clone();
            let mut my_plans = my_plans;
            let bus = bus;
            let auth_inner = auth_for_effect.clone();
            let version_check = refresh_version;
            let effect_lang = locale.lang();
            spawn(async move {
                my_plans.set(SectionState::Loading);
                let res = client.list_my_plans().await;
                if version_check() != version || locale.lang() != effect_lang {
                    return;
                }
                match res {
                    Ok(data) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/users/me/plans");
                        my_plans.set(SectionState::Loaded(data.items));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth_401_only(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/users/me/plans", &err);
                        my_plans.set(SectionState::Error(humanize_error(
                            &err,
                            ErrorContext::PersonalCenter,
                            effect_lang,
                        )));
                    }
                }
            });
        });
    }

    // 可购套餐：初始拉取 + 错误重试（available_version）。购买不改变
    // 模板列表，不依赖 refresh_version；同步读 locale.lang() 会在语言
    // 切换时重拉一次，与姊妹视图一致。
    // 带过期守卫（version / lang 复核）：旧的在途响应一律丢弃。
    {
        let client = auth.client.clone();
        let bus = log_bus;
        let auth_for_effect = auth.clone();
        use_effect(move || {
            // version 同步段读取，理由见"我的套餐" effect 同位置注释。
            let version = available_version();
            let client = client.clone();
            let mut available = available;
            let bus = bus;
            let auth_inner = auth_for_effect.clone();
            let version_check = available_version;
            let effect_lang = locale.lang();
            spawn(async move {
                available.set(SectionState::Loading);
                let res = client.list_available_plans().await;
                if version_check() != version || locale.lang() != effect_lang {
                    return;
                }
                match res {
                    Ok(data) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/plans/available");
                        available.set(SectionState::Loaded(data.items));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth_401_only(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/plans/available", &err);
                        available.set(SectionState::Error(humanize_error(
                            &err,
                            ErrorContext::PersonalCenter,
                            effect_lang,
                        )));
                    }
                }
            });
        });
    }

    // 账单记录：初始 + 翻页/每页数变更 + 购买成功后重拉；
    // 带过期响应丢弃（version / page / per_page / lang 复核）。
    {
        let client = auth.client.clone();
        let bus = log_bus;
        let auth_for_effect = auth.clone();
        use_effect(move || {
            // version 同步段读取，理由见"我的套餐" effect 同位置注释。
            let version = refresh_version();
            let current_page = page();
            let current_per_page = per_page();
            let client = client.clone();
            let mut orders = orders;
            let bus = bus;
            let auth_inner = auth_for_effect.clone();
            let version_check = refresh_version;
            let mut total_signal = total;
            let mut total_pages_signal = total_pages;
            let effect_lang = locale.lang();
            spawn(async move {
                orders.set(SectionState::Loading);
                let res = client.list_my_orders(current_page, current_per_page).await;
                if version_check() != version
                    || page() != current_page
                    || per_page() != current_per_page
                    || locale.lang() != effect_lang
                {
                    return;
                }
                match res {
                    Ok(page_data) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/users/me/orders");
                        total_signal.set(page_data.total);
                        total_pages_signal.set(page_data.total_pages);
                        orders.set(SectionState::Loaded(page_data.items));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth_401_only(&err, auth_inner, nav, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/users/me/orders", &err);
                        orders.set(SectionState::Error(humanize_error(
                            &err,
                            ErrorContext::PersonalCenter,
                            effect_lang,
                        )));
                    }
                }
            });
        });
    }

    let balance_snapshot = balance();
    let balance_error_snapshot = balance_error.read().clone();
    let my_plans_snapshot = my_plans.read().clone();
    let available_snapshot = available.read().clone();
    let orders_snapshot = orders.read().clone();
    let buying_snapshot = purchase.buying_plan.read().clone();
    let submitting_snapshot = *purchase.submitting.read();
    let feedback_ok_snapshot = *purchase.feedback_ok.read();
    let feedback_err_snapshot = purchase.feedback_err.read().clone();
    let dialog_err_snapshot = purchase.dialog_err.read().clone();

    let pagination = PaginationState {
        page: page(),
        per_page: per_page(),
        total: total(),
        total_pages: total_pages(),
        page_signal: page,
        per_page_signal: per_page,
    };

    rsx! {
        // 复用 settings.css 的 ains-settings__section 卡片样式。
        document::Link { rel: "stylesheet", href: asset!("/assets/settings.css") }
        div { class: "ains-users",
            header { class: "ains-users__header",
                div { class: "ains-users__title-block",
                    h1 { class: "ains-users__title", "{t.personal_center_title}" }
                    p { class: "ains-users__subtitle", "{t.personal_center_subtitle}" }
                }
            }

            { render_balance_card(t, balance_snapshot, balance_error_snapshot, balance_error, balance_version) }
            { render_my_plans_section(t, my_plans_snapshot, refresh_version) }
            {
                render_available_section(
                    t,
                    available_snapshot,
                    balance_snapshot,
                    feedback_ok_snapshot,
                    feedback_err_snapshot,
                    purchase,
                    available_version,
                )
            }
            { render_orders_section(t, orders_snapshot, pagination, refresh_version) }
            {
                render_purchase_confirm(
                    locale,
                    buying_snapshot,
                    submitting_snapshot,
                    dialog_err_snapshot,
                    purchase,
                    auth.client.clone(),
                    log_bus,
                    auth.clone(),
                    nav,
                )
            }
        }
    }
}

/// 区块 Loading 态占位（四个区块共用，含余额卡片）。
fn render_section_loading(t: &'static Translations) -> Element {
    rsx! {
        div { class: "ains-users__status",
            LoaderCircle { class: "ains-btn__spinner" }
            "{t.pc_loading}"
        }
    }
}

/// 区块 Error 态横幅 + 重试按钮：点击递增对应驱动信号重拉。
/// 我的套餐与账单共用 refresh_version（与购买成功后的双区块刷新
/// 同源，重试其一会顺带重拉另一个 —— 两个 GET 都很轻，接受冗余
/// 换简单）；可购套餐用独立的 available_version。
/// （余额卡片的错误横幅带独立清错逻辑，不走本函数。）
fn render_section_error(
    t: &'static Translations,
    msg: &str,
    mut retry_version: Signal<u64>,
) -> Element {
    let retry_handler = move |_: MouseEvent| {
        retry_version.with_mut(|v| *v += 1);
    };
    rsx! {
        div { class: "ains-users__status ains-users__status--error",
            TriangleAlert {}
            "{msg}"
            button {
                class: "ains-btn ains-btn--secondary",
                r#type: "button",
                onclick: retry_handler,
                "{t.pc_retry_btn}"
            }
        }
    }
}

/// 余额卡片：大字余额 + 充值提示（一期无支付网关，引导联系管理员）。
/// 加载失败时提供重试入口 —— 否则购买按钮会因余额未知而永久置灰，
/// 用户只能整页刷新恢复。
fn render_balance_card(
    t: &'static Translations,
    balance: Option<i64>,
    error: Option<String>,
    mut balance_error: Signal<Option<String>>,
    mut balance_version: Signal<u64>,
) -> Element {
    let retry_handler = move |_: MouseEvent| {
        // 清错误回到 loading 态，递增版本重驱余额 effect。
        balance_error.set(None);
        balance_version.with_mut(|v| *v += 1);
    };
    rsx! {
        section { class: "ains-settings__section",
            h2 { class: "ains-settings__section-title", "{t.pc_balance_title}" }
            // 错误横幅与余额展示不互斥：若此前已加载过余额（如切换
            // 语言触发的重拉失败），继续展示最后已知余额，避免瞬时
            // 错误遮蔽可用数据（购买按钮仍由 balance 信号驱动）。
            if let Some(err) = error.as_ref() {
                div { class: "ains-users__status ains-users__status--error",
                    TriangleAlert {}
                    "{err}"
                    button {
                        class: "ains-btn ains-btn--secondary",
                        r#type: "button",
                        onclick: retry_handler,
                        "{t.pc_retry_btn}"
                    }
                }
            }
            if let Some(stored) = balance {
                div { class: "ains-users__title ains-table__mono",
                    Wallet {}
                    " {format_balance(stored)}"
                }
                p { class: "ains-settings__desc", "{t.pc_balance_topup_hint}" }
            } else if error.is_none() {
                { render_section_loading(t) }
            }
        }
    }
}

/// 我的套餐：实例列表（剩余次数 / 到期时间 / 来源 / 派生状态）。
fn render_my_plans_section(
    t: &'static Translations,
    state: SectionState<UserPlanResponse>,
    retry_version: Signal<u64>,
) -> Element {
    let body = match state {
        SectionState::Loading => render_section_loading(t),
        SectionState::Error(msg) => render_section_error(t, &msg, retry_version),
        SectionState::Loaded(items) => {
            let columns = vec![
                Column::new(t.orders_column_plan).align(Align::Left),
                Column::new(t.pc_col_remaining)
                    .width("w-28")
                    .align(Align::Right),
                Column::new(t.pc_col_expires)
                    .width("w-40")
                    .align(Align::Left),
                Column::new(t.pc_col_source)
                    .width("w-24")
                    .align(Align::Center),
                Column::new(t.orders_column_status)
                    .width("w-24")
                    .align(Align::Center),
            ];
            let rows: Vec<Element> = items
                .into_iter()
                .map(|inst| {
                    let id = inst.id.clone();
                    let name = inst.plan_name.clone();
                    let remaining = format!("{} / {}", inst.remaining_calls, inst.total_calls);
                    let expires = format_dt(&inst.expires_at);
                    let source = plan_source_label(t, &inst.source);
                    let status = inst.status.clone();
                    let status_text = plan_status_label(t, &status);
                    rsx! {
                        tr { key: "{id}",
                            td {
                                span { class: "ains-table__name-cell", "{name}" }
                            }
                            td { class: "ains-table__mono ains-table__align--right", "{remaining}" }
                            td { class: "ains-table__mono", "{expires}" }
                            td { "{source}" }
                            td {
                                Badge {
                                    variant: plan_status_variant(&status),
                                    "{status_text}"
                                }
                            }
                        }
                    }
                })
                .collect();
            rsx! {
                div { class: "ains-users__table-wrapper",
                    DataTable {
                        columns,
                        rows,
                        empty: Some(rsx! { "{t.pc_my_plans_empty}" }),
                    }
                }
            }
        }
    };
    rsx! {
        section { class: "ains-settings__section",
            h2 { class: "ains-settings__section-title", "{t.pc_my_plans_title}" }
            {body}
        }
    }
}

/// 可购套餐：模板列表 + 购买入口。余额不足或余额未加载时按钮禁用
/// （存储单位 i64 比较，服务端 400 仍兜底）。
fn render_available_section(
    t: &'static Translations,
    state: SectionState<PlanResponse>,
    balance: Option<i64>,
    feedback_ok: bool,
    feedback_err: Option<String>,
    purchase: PurchaseSignals,
    retry_version: Signal<u64>,
) -> Element {
    let body = match state {
        SectionState::Loading => render_section_loading(t),
        SectionState::Error(msg) => render_section_error(t, &msg, retry_version),
        SectionState::Loaded(items) => {
            let columns = vec![
                Column::new(t.plans_column_name).align(Align::Left),
                Column::new(t.plans_column_price)
                    .width("w-24")
                    .align(Align::Right),
                Column::new(t.plans_column_calls)
                    .width("w-24")
                    .align(Align::Right),
                Column::new(t.plans_column_validity)
                    .width("w-24")
                    .align(Align::Right),
                Column::new(t.plans_column_actions)
                    .width("w-28")
                    .align(Align::Center),
            ];
            let rows: Vec<Element> = items
                .into_iter()
                .map(|plan| {
                    let id = plan.id.clone();
                    let name = plan.name.clone();
                    let description = plan.description.clone();
                    let price_display = format_balance(plan.price);
                    let affordable = buy_button_enabled(balance, plan.price);
                    let buy_label = if show_insufficient_label(balance, plan.price) {
                        t.pc_buy_insufficient
                    } else {
                        t.pc_buy_btn
                    };

                    let p_for_buy = plan.clone();
                    let mut s_buy = purchase;
                    let buy_handler = move |_: MouseEvent| {
                        s_buy.feedback_ok.set(false);
                        s_buy.feedback_err.set(None);
                        s_buy.dialog_err.set(None);
                        s_buy.buying_plan.set(Some(p_for_buy.clone()));
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
                            td { class: "ains-table__mono ains-table__align--right", "{price_display}" }
                            td { class: "ains-table__mono ains-table__align--right", "{plan.total_calls}" }
                            td { class: "ains-table__mono ains-table__align--right", "{plan.validity_days}" }
                            td {
                                button {
                                    class: "ains-btn ains-btn--primary",
                                    r#type: "button",
                                    disabled: !affordable,
                                    onclick: buy_handler,
                                    "{buy_label}"
                                }
                            }
                        }
                    }
                })
                .collect();
            rsx! {
                div { class: "ains-users__table-wrapper",
                    DataTable {
                        columns,
                        rows,
                        empty: Some(rsx! { "{t.pc_available_empty}" }),
                    }
                }
            }
        }
    };
    rsx! {
        section { class: "ains-settings__section",
            h2 { class: "ains-settings__section-title", "{t.pc_available_title}" }
            if feedback_ok {
                p { class: "ains-form-success", "{t.pc_buy_success}" }
            }
            if let Some(err) = feedback_err.as_ref() {
                p { class: "ains-form-error", "{err}" }
            }
            {body}
        }
    }
}

/// 账单记录：分页表格（分页行为与 plans/orders 管理页对齐）。
fn render_orders_section(
    t: &'static Translations,
    state: SectionState<PaymentOrderResponse>,
    pagination: PaginationState,
    retry_version: Signal<u64>,
) -> Element {
    let body = match state {
        SectionState::Loading => render_section_loading(t),
        SectionState::Error(msg) => render_section_error(t, &msg, retry_version),
        SectionState::Loaded(items) => {
            let columns = vec![
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
            ];
            let rows: Vec<Element> = items
                .into_iter()
                .map(|order| {
                    let id = order.id.clone();
                    let plan_name = if order.plan_name.is_empty() {
                        "-".to_string()
                    } else {
                        order.plan_name.clone()
                    };
                    let amount = format_balance(order.amount);
                    let method = order_method_label(t, &order.payment_method);
                    let is_paid = order.status == "paid";
                    let status_text = order_status_label(t, &order.status);
                    let created = format_dt(&order.created_at);
                    rsx! {
                        tr { key: "{id}",
                            td {
                                span { class: "ains-table__name-cell", "{plan_name}" }
                            }
                            td { class: "ains-table__mono ains-table__align--right", "{amount}" }
                            td { "{method}" }
                            td {
                                if is_paid {
                                    Badge { variant: BadgeVariant::Success, "{status_text}" }
                                } else {
                                    Badge { variant: BadgeVariant::Admin, "{status_text}" }
                                }
                            }
                            td { class: "ains-table__mono", "{created}" }
                        }
                    }
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
                prev_sig.set(prev_page(page));
            };
            let mut next_sig = page_signal;
            let on_next = move |_: MouseEvent| {
                next_sig.set(next_page(page, total_pages));
            };
            // 账单文案用"记录"计数（metering 键），而非用户/订单管理页的实体计数。
            let pagination_info = if total_pages == 0 {
                tf(t.metering_count_simple, &[("total", &total.to_string())])
            } else {
                tf(
                    t.metering_count_info,
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
                        empty: Some(rsx! { "{t.pc_orders_empty}" }),
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
    };
    rsx! {
        section { class: "ains-settings__section",
            h2 { class: "ains-settings__section-title", "{t.pc_orders_title}" }
            {body}
        }
    }
}

/// 购买确认弹窗 + 提交链路。
///
/// submitting 期间确认按钮禁用（UI 防双击）；服务端 per-user 购买锁
/// （409 purchase_in_progress）是最终兜底。成功后余额取服务端返回值。
///
/// 接收 `locale`（Copy）而非 `t`：错误文案的语言在**点击时**通过
/// `locale.lang()` 读取，避免在事件处理器内调用 `use_context`（渲染
/// 阶段之外注册 hook 槽位，依赖脆弱的槽位顺序行为）。
#[allow(clippy::too_many_arguments)]
fn render_purchase_confirm(
    locale: I18nContext,
    buying: Option<PlanResponse>,
    submitting: bool,
    dialog_err: Option<String>,
    purchase: PurchaseSignals,
    client: client_api::Client,
    log_bus: LogBus,
    auth: AuthState,
    nav: dioxus::prelude::dioxus_router::Navigator,
) -> Element {
    let t = locale.t();
    let Some(plan) = buying else {
        return VNode::empty();
    };

    let balance_opt = *purchase.balance.read();
    let balance_text = balance_opt
        .map(format_balance)
        .unwrap_or_else(|| "-".to_string());
    let price_text = format_balance(plan.price);
    let message = tf(
        t.pc_confirm_msg,
        &[
            ("name", &plan.name),
            ("price", &price_text),
            ("balance", &balance_text),
        ],
    );

    let mut s_cancel = purchase;
    let on_cancel = move |_: MouseEvent| {
        if *s_cancel.submitting.read() {
            return;
        }
        s_cancel.buying_plan.set(None);
    };

    let mut s_async = purchase;
    let c_async = client;
    let b_async = log_bus;
    let a_async = auth;
    let on_confirm = move |_: MouseEvent| {
        if *s_async.submitting.read() {
            return;
        }
        let Some(plan) = s_async.buying_plan.cloned() else {
            return;
        };
        let target_id = plan.id.clone();
        s_async.submitting.set(true);
        // 重试（409 后再次确认）前清除弹窗内联错误，提交中只展示加载态。
        s_async.dialog_err.set(None);
        let client_async = c_async.clone();
        let bus_async = b_async;
        let mut s_inner = s_async;
        let auth_async = a_async.clone();
        // 点击时读当前语言（事件处理器内读信号不产生订阅）。
        let lang = locale.lang();
        spawn(async move {
            let res = client_async.purchase_plan(&target_id).await;
            s_inner.submitting.set(false);
            match res {
                Ok(outcome) => {
                    push_log_ok(
                        bus_async,
                        HttpMethod::Post,
                        &format!("/api/plans/{target_id}/purchase"),
                    );
                    // 信号编排集中在 apply_purchase_success（余额直更 +
                    // epoch 作废在途 get_me + 双区块刷新，详见其文档）。
                    apply_purchase_success(s_inner, outcome.balance);
                    // 成功横幅 TTL 清除：同一任务内继续 sleep（避免嵌套
                    // spawn）；清除前复核 feedback_seq，若期间又有新购买
                    // 成功（seq 已变）则不动新横幅。
                    let seq = *s_inner.feedback_seq.read();
                    client_api::Client::sleep_ms(SUCCESS_BANNER_TTL_MS).await;
                    if *s_inner.feedback_seq.read() == seq {
                        s_inner.feedback_ok.set(false);
                    }
                }
                Err(err) => {
                    if crate::api::handle_unauth_401_only(&err, auth_async, nav, bus_async).await {
                        return;
                    }
                    push_log_err(
                        bus_async,
                        HttpMethod::Post,
                        &format!("/api/plans/{target_id}/purchase"),
                        &err,
                    );
                    let msg = humanize_error(&err, ErrorContext::PersonalCenter, lang);
                    apply_purchase_failure(s_inner, msg, purchase_error_keeps_dialog_open(&err));
                }
            }
        });
    };

    rsx! {
        ConfirmDialog {
            open: true,
            title: t.pc_confirm_title.to_string(),
            message,
            error: dialog_err,
            loading: submitting,
            confirm_label: t.pc_confirm_btn.to_string(),
            on_confirm,
            on_cancel,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PurchaseSignals, apply_purchase_failure, apply_purchase_success, balance_response_is_stale,
        buy_button_enabled, can_afford, next_page, plan_source_label, plan_status_label,
        plan_status_variant, prev_page, purchase_error_keeps_dialog_open, show_insufficient_label,
    };
    use crate::balance::BALANCE_SCALE;
    use client_api::{ClientError, PlanResponse};
    use dioxus::prelude::*;
    use dioxus_core::VirtualDom;
    use i18n::{EN, ZH};
    use std::sync::atomic::{AtomicBool, Ordering};
    use ui::BadgeVariant;

    #[test]
    fn can_afford_exact_price_is_allowed() {
        // 余额恰好等于价格：允许购买（购后余额归零）。
        assert!(can_afford(BALANCE_SCALE * 4, BALANCE_SCALE * 4));
    }

    #[test]
    fn can_afford_insufficient_is_rejected() {
        assert!(!can_afford(BALANCE_SCALE * 4 - 1, BALANCE_SCALE * 4));
        assert!(!can_afford(0, 1));
    }

    #[test]
    fn can_afford_ample_balance_is_allowed() {
        assert!(can_afford(i64::MAX, 1));
        // 全 i64 范围内为精确整数比较，无 f64 精度问题。
        assert!(can_afford(i64::MAX, i64::MAX));
    }

    #[test]
    fn buy_button_disabled_while_balance_unknown() {
        // 余额未加载（None）：按钮禁用，但不得显示“余额不足”——
        // 余额未知 ≠ 余额不足，误报会让用户无谓去充值。
        assert!(!buy_button_enabled(None, BALANCE_SCALE));
        assert!(!show_insufficient_label(None, BALANCE_SCALE));
    }

    #[test]
    fn buy_button_states_with_known_balance() {
        // 余额充足：可买，无“余额不足”。
        assert!(buy_button_enabled(Some(BALANCE_SCALE * 2), BALANCE_SCALE));
        assert!(!show_insufficient_label(
            Some(BALANCE_SCALE * 2),
            BALANCE_SCALE
        ));
        // 余额不足：禁用且显示“余额不足”。
        assert!(!buy_button_enabled(Some(BALANCE_SCALE - 1), BALANCE_SCALE));
        assert!(show_insufficient_label(
            Some(BALANCE_SCALE - 1),
            BALANCE_SCALE
        ));
        // 临界：余额恰好等于价格 → 可买。
        assert!(buy_button_enabled(Some(BALANCE_SCALE), BALANCE_SCALE));
    }

    #[test]
    fn pagination_boundaries_never_produce_invalid_pages() {
        // 首页继续向前停在第 1 页；末页继续向后停在末页。
        assert_eq!(prev_page(1), 1);
        assert_eq!(prev_page(2), 1);
        assert_eq!(next_page(5, 5), 5);
        assert_eq!(next_page(4, 5), 5);
        // total_pages == 0（空列表）时按钮虽被禁用，函数本身也
        // 绝不产生非法的第 0 页。
        assert_eq!(next_page(1, 0), 1);
        assert_eq!(prev_page(0), 1);
        // 全 u64 域无 panic（saturating_add）：极端页码不溢出。
        assert_eq!(next_page(u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(next_page(u64::MAX, 5), 5);
    }

    #[test]
    fn balance_staleness_guard_discards_superseded_responses() {
        // 无并发变化：响应有效，允许写入。
        assert!(!balance_response_is_stale(1, 0, 1, 0));
        // 在途期间用户点了重试（version 变化）：旧响应作废。
        assert!(balance_response_is_stale(1, 0, 2, 0));
        // 在途期间购买成功直更余额（epoch 变化）：旧响应作废，
        // 不得用购买前的陈旧余额覆盖权威值。
        assert!(balance_response_is_stale(1, 0, 1, 1));
        // 两者同时变化。
        assert!(balance_response_is_stale(1, 0, 2, 1));
    }

    #[test]
    fn unknown_plan_status_falls_back_neutrally() {
        // 已知状态走本地化文案。
        assert_eq!(plan_status_label(&EN, "active"), EN.pc_plan_status_active);
        assert_eq!(plan_status_label(&EN, "expired"), EN.pc_plan_status_expired);
        // ZH 抽查：守卫 translate! 宏的 EN/ZH 字段映射不错位。
        assert_eq!(plan_status_label(&ZH, "active"), ZH.pc_plan_status_active);
        assert_ne!(ZH.pc_plan_status_active, EN.pc_plan_status_active);
        assert_eq!(
            plan_source_label(&ZH, "purchase"),
            ZH.pc_plan_source_purchase
        );
        assert_ne!(ZH.pc_plan_source_purchase, EN.pc_plan_source_purchase);
        // 未知状态原样透出 + 灰色徽章 —— 不得回退成“生效中”文案。
        assert_eq!(plan_status_label(&EN, "suspended"), "suspended");
        assert!(matches!(
            plan_status_variant("suspended"),
            BadgeVariant::User
        ));
        assert!(matches!(
            plan_status_variant("active"),
            BadgeVariant::Success
        ));
        assert!(matches!(
            plan_status_variant("exhausted"),
            BadgeVariant::Warning
        ));
    }

    // ── 购买失败的弹窗保持判定 ────────────────────

    #[test]
    fn only_purchase_in_progress_409_keeps_dialog_open() {
        // 可重试：409 + purchase_in_progress —— 保持弹窗。
        let retryable = ClientError::Other(409, r#"{"error":"purchase_in_progress"}"#.into());
        assert!(purchase_error_keeps_dialog_open(&retryable));

        // 终态错误一律关闭弹窗：余额不足 / 套餐下架 / 租户禁用。
        let terminal = [
            ClientError::Other(400, r#"{"error":"insufficient_balance"}"#.into()),
            ClientError::Other(404, r#"{"error":"not_found"}"#.into()),
            ClientError::Other(403, r#"{"error":"forbidden"}"#.into()),
            ClientError::Network("timeout".into()),
        ];
        for err in &terminal {
            assert!(!purchase_error_keeps_dialog_open(err), "err: {err:?}");
        }

        // 409 但错误码不是 purchase_in_progress（如 conflict）或体
        // 非 JSON：不是可重试锁冲突，不得保持弹窗。
        let conflict = ClientError::Other(409, r#"{"error":"conflict"}"#.into());
        assert!(!purchase_error_keeps_dialog_open(&conflict));
        let garbled = ClientError::Other(409, "not json".into());
        assert!(!purchase_error_keeps_dialog_open(&garbled));
    }

    // ── 购买成功/失败的信号编排（VirtualDom 内创建信号，
    //    AtomicBool 作 side channel，与 frontend_test.rs 策略一致）──

    fn dummy_plan() -> PlanResponse {
        serde_json::from_value(serde_json::json!({
            "id": "p1",
            "tenant_id": "default",
            "name": "Starter",
            "price": 100i64,
            "total_calls": 10,
            "validity_days": 30,
            "status": "active",
            "created_at": "2024-01-15T08:00:00Z",
            "updated_at": "2024-01-15T08:00:00Z",
        }))
        .expect("valid plan json")
    }

    /// 从“脏”初始态出发构造购买信号组：弹窗打开、新旧错误并存、
    /// 账单停在第 3 页 —— 用于验证编排 helper 的清理完整性。
    fn dirty_signals() -> PurchaseSignals {
        PurchaseSignals {
            buying_plan: Signal::new(Some(dummy_plan())),
            submitting: Signal::new(false),
            feedback_ok: Signal::new(false),
            feedback_err: Signal::new(Some("old section err".to_string())),
            dialog_err: Signal::new(Some("old dialog err".to_string())),
            balance: Signal::new(Some(500)),
            balance_epoch: Signal::new(0),
            orders_page: Signal::new(3),
            refresh_version: Signal::new(7),
            feedback_seq: Signal::new(0),
        }
    }

    #[test]
    fn purchase_success_choreography() {
        static ALL_OK: AtomicBool = AtomicBool::new(false);

        let mut dom = VirtualDom::new(|| {
            let s = dirty_signals();
            apply_purchase_success(s, 400);

            let ok = *s.balance.read() == Some(400)          // 余额直取返回值
                && *s.balance_epoch.read() == 1              // 作废在途 get_me
                && s.buying_plan.read().is_none()            // 弹窗关闭
                && s.dialog_err.read().is_none()             // 弹窗错误清除
                && s.feedback_err.read().is_none()           // 区块错误清除
                && *s.feedback_ok.read()                     // 成功横幅点亮
                && *s.feedback_seq.read() == 1               // 横幅代际递增
                && *s.orders_page.read() == 1                // 账单回第 1 页
                && *s.refresh_version.read() == 8; // 双区块重拉
            ALL_OK.store(ok, Ordering::SeqCst);
            rsx! {
                div {}
            }
        });
        dom.rebuild_in_place();

        assert!(
            ALL_OK.load(Ordering::SeqCst),
            "购买成功后的信号编排不完整（余额/epoch/弹窗/横幅/分页/刷新）"
        );
    }

    #[test]
    fn purchase_failure_terminal_closes_dialog_and_shows_section_error() {
        static ALL_OK: AtomicBool = AtomicBool::new(false);

        let mut dom = VirtualDom::new(|| {
            let s = dirty_signals();
            apply_purchase_failure(s, "余额不足".to_string(), false);

            let ok = s.buying_plan.read().is_none()          // 终态错误：弹窗关闭
                && s.dialog_err.read().is_none()             // 弹窗错误清空（互斥）
                && s.feedback_err.read().as_deref() == Some("余额不足")
                && !*s.feedback_ok.read()
                // 失败不得副作用刷新/翻页/动余额：
                && *s.orders_page.read() == 3
                && *s.refresh_version.read() == 7
                && *s.balance.read() == Some(500)
                && *s.balance_epoch.read() == 0;
            ALL_OK.store(ok, Ordering::SeqCst);
            rsx! {
                div {}
            }
        });
        dom.rebuild_in_place();

        assert!(
            ALL_OK.load(Ordering::SeqCst),
            "终态失败应关闭弹窗并仅展示区块错误，无其他副作用"
        );
    }

    #[test]
    fn purchase_failure_retryable_keeps_dialog_with_inline_error() {
        static ALL_OK: AtomicBool = AtomicBool::new(false);

        let mut dom = VirtualDom::new(|| {
            let s = dirty_signals();
            apply_purchase_failure(s, "购买处理中".to_string(), true);

            let ok = s.buying_plan.read().is_some()          // 可重试：弹窗保持打开
                && s.dialog_err.read().as_deref() == Some("购买处理中")
                && s.feedback_err.read().is_none()           // 区块错误清空（互斥）
                && !*s.feedback_ok.read()
                && *s.orders_page.read() == 3
                && *s.refresh_version.read() == 7;
            ALL_OK.store(ok, Ordering::SeqCst);
            rsx! {
                div {}
            }
        });
        dom.rebuild_in_place();

        assert!(
            ALL_OK.load(Ordering::SeqCst),
            "可重试失败（409）应保持弹窗打开并仅内联展示错误"
        );
    }
}
