//! Token 用量统计视图（admin/system）。
//!
//! 上半部分展示概览统计卡片，下半部分提供可筛选的分页用量数据表。

use chrono::{DateTime, Utc};
use client_api::{TokenUsageResponse, UsageStatsResponse};
use dioxus::prelude::*;
use dioxus_icons::lucide::{Activity, Gauge, LoaderCircle, TriangleAlert};

use ui::{
    Align, Badge, BadgeVariant, Button, ButtonType, Column, DataTable, I18nContext, StatsAccent,
    StatsCard, Translations, tf,
};

use crate::api::{ErrorContext, humanize_error};
use crate::auth::AuthState;
use crate::components::{HttpMethod, LogBus, SearchSignal, push_log_err, push_log_ok};
use client_api::ListUsageFilter;

#[derive(Debug, Clone)]
enum UsageListState {
    Loading,
    Loaded(Vec<TokenUsageResponse>),
    Error(String),
}

#[derive(Debug, Clone, Default)]
struct FilterState {
    date_from: String,
    date_to: String,
    model: String,
}

#[component]
pub fn Metering() -> Element {
    let auth = use_context::<AuthState>();
    let log_bus = use_context::<LogBus>();
    let nav = use_navigator();
    let locale = use_context::<I18nContext>();
    let t = locale.t();

    let list = use_signal(|| UsageListState::Loading);
    let fetch_version = use_signal(|| 0u64);
    let total = use_signal(|| 0u64);
    let total_pages = use_signal(|| 0u64);
    let page = use_signal(|| 1u64);
    let per_page = use_signal(|| 20u64);
    let stats = use_signal(|| Option::<UsageStatsResponse>::None);
    let stats_loading = use_signal(|| false);
    let filters = use_signal(FilterState::default);
    let SearchSignal(_search_query) = use_context::<SearchSignal>();

    // 数据拉取 — list 和 stats 在同一 spawn 内顺序获取，
    // 用统一的 fetch_version 做版本检查，避免两个独立 spawn 的竞态窗口。
    {
        let client = auth.client.clone();
        let bus = log_bus;
        let auth_for_effect = auth.clone();
        let nav_for_effect = nav;
        use_effect(move || {
            let version = fetch_version();
            let current_page = page();
            let current_per_page = per_page();
            let client = client.clone();
            let mut list = list;
            let mut total_signal = total;
            let mut total_pages_signal = total_pages;
            let mut stats_signal = stats;
            let mut stats_loading_signal = stats_loading;
            let bus = bus;
            let auth_inner = auth_for_effect.clone();
            let version_check = fetch_version;
            let effect_lang = locale.lang();
            let nav_async = nav_for_effect;
            let auth_for_stats = auth_inner.clone();
            spawn(async move {
                list.set(UsageListState::Loading);
                stats_loading_signal.set(true);

                // 在 async 上下文内读取 filters：不会被 use_effect 追踪为依赖，
                // 因此筛选框输入不会每敲一个字符就发请求，
                // 仅由 fetch_version（点击 Apply 时递增）驱动拉取。
                let current_filters = filters.cloned();
                let filter = build_filter(&current_filters);
                let lang = effect_lang;

                // Fetch list first, then stats (sequential to avoid interleaving).
                // Both use the same version guard — if version changes between
                // the two await points, the second result is discarded.
                let list_res = client
                    .list_usage(current_page, current_per_page, filter.as_ref())
                    .await;

                // Guard: if a newer fetch superseded this one (version / page /
                // per_page changed while awaiting), abort early. The newer task
                // now owns the loading state — including `stats_loading` — so we
                // must neither clear it nor fetch stats for this stale request,
                // otherwise we'd prematurely hide the newer fetch's indicator.
                if version_check() != version
                    || page() != current_page
                    || per_page() != current_per_page
                {
                    return;
                }
                match list_res {
                    Ok(data) => {
                        push_log_ok(bus, HttpMethod::Get, "/api/usage");
                        total_signal.set(data.total);
                        total_pages_signal.set(data.total_pages);
                        list.set(UsageListState::Loaded(data.items));
                    }
                    Err(err) => {
                        if crate::api::handle_unauth(&err, auth_inner, nav_async, bus).await {
                            return;
                        }
                        push_log_err(bus, HttpMethod::Get, "/api/usage", &err);
                        list.set(UsageListState::Error(humanize_error(
                            &err,
                            ErrorContext::Metering,
                            lang,
                        )));
                    }
                }

                // Fetch stats (after list, but still within the same version window).
                let s_res = client.get_usage_stats(filter.as_ref()).await;

                // Same version guard — if a newer fetch started while awaiting
                // stats, discard this result and leave `stats_loading` to the
                // newer task; only clear the indicator when still current.
                if version_check() == version
                    && page() == current_page
                    && per_page() == current_per_page
                {
                    stats_loading_signal.set(false);
                    match s_res {
                        Ok(s) => stats_signal.set(Some(s)),
                        Err(e) => {
                            if crate::api::handle_unauth(&e, auth_for_stats, nav_for_effect, bus)
                                .await
                            {
                                return;
                            }
                            push_log_err(bus, HttpMethod::Get, "/api/usage/stats", &e);
                            stats_signal.set(None);
                        }
                    }
                }
            });
        });
    }

    let list_snapshot = list.cloned();
    let page_val = page();
    let per_page_val = per_page();
    let total_val = total();
    let total_pages_val = total_pages();
    let stats_snapshot = stats.cloned();
    let stats_loading_val = *stats_loading.read();
    let filters_snapshot = filters.cloned();

    // Filter event handlers
    let mut f_date_from = filters;
    let on_date_from = move |evt: FormEvent| f_date_from.write().date_from = evt.value();
    let mut f_date_to = filters;
    let on_date_to = move |evt: FormEvent| f_date_to.write().date_to = evt.value();
    let mut f_model = filters;
    let on_model = move |evt: FormEvent| f_model.write().model = evt.value();

    // 点击日期输入框任意位置时主动弹出原生日历选择器。
    // 仅靠原生行为在部分浏览器/WebView 下点击文本区不会弹出日历，
    // 通过 showPicker() 显式触发可保证鼠标点击即可选择日期；
    // typeof 守卫兼容不支持 showPicker 的环境，try/catch 吞掉
    // "picker 已显示" 等异常。
    let on_date_from_pick = move |_: MouseEvent| {
        spawn(async move {
            let _ = document::eval(
                "const el = document.getElementById('mg-date-from'); \
                 if (el && typeof el.showPicker === 'function') { try { el.showPicker(); } catch (e) {} }",
            )
            .await;
        });
    };
    let on_date_to_pick = move |_: MouseEvent| {
        spawn(async move {
            let _ = document::eval(
                "const el = document.getElementById('mg-date-to'); \
                 if (el && typeof el.showPicker === 'function') { try { el.showPicker(); } catch (e) {} }",
            )
            .await;
        });
    };

    let mut fv_apply = fetch_version;
    let mut page_apply = page;
    let on_apply = move |_: MouseEvent| {
        fv_apply.with_mut(|v| *v += 1);
        page_apply.set(1);
    };
    let mut fv_reset = fetch_version;
    let mut page_reset = page;
    let mut f_reset = filters;
    let on_reset = move |_: MouseEvent| {
        f_reset.set(FilterState::default());
        fv_reset.with_mut(|v| *v += 1);
        page_reset.set(1);
    };

    // Stats data
    let prompt_display = match &stats_snapshot {
        Some(s) => format_display_tokens(s.total_prompt_tokens),
        None => "—".to_string(),
    };
    let completion_display = match &stats_snapshot {
        Some(s) => format_display_tokens(s.total_completion_tokens),
        None => "—".to_string(),
    };
    let stats_sub_label = match &stats_snapshot {
        Some(_) => tf(
            t.metering_stats_prompt_completion,
            &[
                ("prompt", &prompt_display),
                ("completion", &completion_display),
            ],
        ),
        None => "—".to_string(),
    };
    let (total_requests, total_tokens_str, model_count_str) = match &stats_snapshot {
        Some(s) => (
            s.total_requests.to_string(),
            format_display_tokens(s.total_tokens),
            s.model_breakdown.len().to_string(),
        ),
        None => ("—".to_string(), "—".to_string(), "—".to_string()),
    };

    rsx! {
        div { class: "ains-users",
            header { class: "ains-users__header",
                div { class: "ains-users__title-block",
                    h1 { class: "ains-users__title", "{t.metering_title}" }
                    p { class: "ains-users__subtitle", "{t.metering_subtitle}" }
                }
            }

            // Stats cards
            section { class: "ains-stats-grid",
                if stats_loading_val && stats_snapshot.is_none() {
                    div { class: "ains-users__status",
                        LoaderCircle { class: "ains-btn__spinner" }
                        "{t.metering_loading}"
                    }
                } else {
                    StatsCard {
                        label: t.metering_stats_total_requests.to_string(),
                        value: total_requests,
                        sub: t.metering_stats_requests_sub.to_string(),
                        icon: rsx! {
                            Activity {}
                        },
                        accent: StatsAccent::Indigo,
                    }
                    StatsCard {
                        label: t.metering_stats_total_tokens.to_string(),
                        value: total_tokens_str,
                        sub: stats_sub_label,
                        icon: rsx! {
                            Gauge {}
                        },
                        accent: StatsAccent::Purple,
                    }
                    StatsCard {
                        label: t.metering_stats_active_models.to_string(),
                        value: model_count_str,
                        sub: t.metering_stats_models_sub.to_string(),
                        icon: rsx! {
                            Activity {}
                        },
                        accent: StatsAccent::Emerald,
                    }
                }
            }

            // Filter bar
            div {
                class: "ains-mg-filter-bar",
                style: "display:flex;flex-wrap:wrap;gap:12px;align-items:end;margin:16px 0;",
                div { style: "display:flex;flex-direction:column;gap:4px;",
                    label {
                        class: "ains-mg-filter-label",
                        style: "font-size:12px;color:oklch(0.6 0 0);text-transform:uppercase;letter-spacing:0.05em;",
                        "{t.metering_filter_date_from}"
                    }
                    input {
                        id: "mg-date-from",
                        r#type: "date",
                        class: "ains-mg-filter-input",
                        style: "padding:8px 12px;border-radius:8px;border:1px solid oklch(0.8 0 0);color:oklch(0.95 0 0);font-size:14px;color-scheme:dark;cursor:pointer;",
                        value: filters_snapshot.date_from.clone(),
                        oninput: on_date_from,
                        onclick: on_date_from_pick,
                    }
                }
                div { style: "display:flex;flex-direction:column;gap:4px;",
                    label {
                        class: "ains-mg-filter-label",
                        style: "font-size:12px;color:oklch(0.6 0 0);text-transform:uppercase;letter-spacing:0.05em;",
                        "{t.metering_filter_date_to}"
                    }
                    input {
                        id: "mg-date-to",
                        r#type: "date",
                        class: "ains-mg-filter-input",
                        style: "padding:8px 12px;border-radius:8px;border:1px solid oklch(0.8 0 0);color:oklch(0.95 0 0);font-size:14px;color-scheme:dark;cursor:pointer;",
                        value: filters_snapshot.date_to.clone(),
                        oninput: on_date_to,
                        onclick: on_date_to_pick,
                    }
                }
                div { style: "display:flex;flex-direction:column;gap:4px;",
                    label {
                        class: "ains-mg-filter-label",
                        style: "font-size:12px;color:oklch(0.6 0 0);text-transform:uppercase;letter-spacing:0.05em;",
                        "{t.metering_filter_model}"
                    }
                    input {
                        r#type: "text",
                        class: "ains-mg-filter-input",
                        placeholder: t.metering_filter_model_placeholder.to_string(),
                        style: "padding:8px 12px;border-radius:8px;border:1px solid oklch(0.8 0 0);color:oklch(0.95 0 0);font-size:14px;min-width:180px;color-scheme:dark;",
                        value: filters_snapshot.model.clone(),
                        oninput: on_model,
                    }
                }
                Button { button_type: ButtonType::Button, onclick: on_apply, "{t.metering_filter_apply}" }
                Button { button_type: ButtonType::Button, onclick: on_reset, "{t.metering_filter_reset}" }
            }

            // Usage table
            {
                render_table(
                    t,
                    list_snapshot,
                    page_val,
                    per_page_val,
                    total_val,
                    total_pages_val,
                    page,
                    per_page,
                    fetch_version,
                )
            }
        }
    }
}

fn build_filter(fs: &FilterState) -> Option<ListUsageFilter> {
    let model = if fs.model.is_empty() {
        None
    } else {
        Some(fs.model.clone())
    };
    // Send bare date strings (e.g. "2026-07-31") and let the backend's
    // `parse_date` expand them: date_from → start-of-day (00:00:00),
    // date_to → end-of-day (23:59:59.999999).  This avoids losing the
    // last 999 ms of the target day, which would happen if we sent a
    // hard-coded "T23:59:59Z" (no microseconds).
    let date_from = if fs.date_from.is_empty() {
        None
    } else {
        Some(fs.date_from.clone())
    };
    let date_to = if fs.date_to.is_empty() {
        None
    } else {
        Some(fs.date_to.clone())
    };
    if model.is_none() && date_from.is_none() && date_to.is_none() {
        None
    } else {
        Some(ListUsageFilter {
            user_id: None,
            channel_id: None,
            model,
            request_type: None,
            date_from,
            date_to,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn render_table(
    t: &'static Translations,
    list_snapshot: UsageListState,
    page: u64,
    per_page: u64,
    total: u64,
    total_pages: u64,
    mut page_signal: Signal<u64>,
    mut per_page_signal: Signal<u64>,
    mut fetch_version: Signal<u64>,
) -> Element {
    match list_snapshot {
        UsageListState::Loading => rsx! {
            div { class: "ains-users__status",
                LoaderCircle { class: "ains-btn__spinner" }
                "{t.metering_loading}"
            }
        },
        UsageListState::Error(msg) => rsx! {
            div { class: "ains-users__status ains-users__status--error",
                TriangleAlert {}
                "{msg}"
            }
        },
        UsageListState::Loaded(items) => {
            let columns = build_columns(t);
            let rows: Vec<Element> = items.into_iter().map(|r| row_element(t, r)).collect();

            let has_prev = page > 1;
            let has_next = page < total_pages;

            let mut prev_sig = page_signal;
            let on_prev = move |_: MouseEvent| {
                prev_sig.set(page.saturating_sub(1).max(1));
            };
            let mut next_sig = page_signal;
            let on_next = move |_: MouseEvent| {
                let max = total_pages.max(1);
                next_sig.set((page + 1).min(max));
            };
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
                        empty: Some(rsx! { "{t.metering_empty}" }),
                    }
                    div { class: "ains-pagination",
                        div { class: "ains-pagination__info", "{pagination_info}" }
                        div { class: "ains-pagination__controls",
                            button {
                                class: "ains-pagination__btn",
                                disabled: !has_prev,
                                onclick: on_prev,
                                "{t.metering_prev_page}"
                            }
                            button {
                                class: "ains-pagination__btn",
                                disabled: !has_next,
                                onclick: on_next,
                                "{t.metering_next_page}"
                            }
                            div { class: "ains-pagination__per-page",
                                span { "{t.metering_per_page_label}" }
                                select {
                                    class: "ains-pagination__select",
                                    value: "{per_page}",
                                    onchange: move |evt| {
                                        if let Ok(v) = evt.value().parse::<u64>() {
                                            let v = v.clamp(1, 100);
                                            fetch_version.with_mut(|fv| *fv += 1);
                                            per_page_signal.set(v);
                                            page_signal.set(1);
                                        }
                                    },
                                    option { value: "20", "20" }
                                    option { value: "50", "50" }
                                    option { value: "100", "100" }
                                }
                                span { "{t.metering_per_page_unit}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn row_element(_t: &'static Translations, r: TokenUsageResponse) -> Element {
    let id = r.id;
    let created = r.created_at;
    let user_id = r.user_id;
    let model = r.model.clone();
    let request_type = r.request_type.clone();
    let prompt = r.prompt_tokens;
    let completion = r.completion_tokens;
    let total_t = r.total_tokens;

    rsx! {
        tr { key: "{id}",
            td { class: "ains-table__mono", "{format_dt(&created)}" }
            td { class: "ains-table__mono ains-table__align--right", "{user_id}" }
            td { class: "ains-table__mono ains-table__truncate",
                span { title: "{model}", "{model}" }
            }
            td {
                if request_type == "chat" {
                    Badge { variant: BadgeVariant::User, "{request_type}" }
                } else {
                    Badge { variant: BadgeVariant::Admin, "{request_type}" }
                }
            }
            td { class: "ains-table__mono ains-table__align--right", "{prompt}" }
            td { class: "ains-table__mono ains-table__align--right", "{completion}" }
            td { class: "ains-table__mono ains-table__align--right", "{total_t}" }
        }
    }
}

fn build_columns(t: &'static Translations) -> Vec<Column> {
    vec![
        Column::new(t.metering_column_created)
            .width("w-28")
            .align(Align::Left),
        Column::new(t.metering_column_user_id)
            .width("w-20")
            .align(Align::Right),
        Column::new(t.metering_column_model)
            .width("w-40")
            .align(Align::Left),
        Column::new(t.metering_column_request_type)
            .width("w-24")
            .align(Align::Center),
        Column::new(t.metering_column_prompt_tokens)
            .width("w-24")
            .align(Align::Right),
        Column::new(t.metering_column_completion_tokens)
            .width("w-28")
            .align(Align::Right),
        Column::new(t.metering_column_total_tokens)
            .width("w-28")
            .align(Align::Right),
    ]
}

fn format_display_tokens(val: i64) -> String {
    // 负数先剥离符号再千分位分组，避免负号被计入长度导致
    // 输出形如 "-,123,456"。使用 unsigned_abs 防止 i64::MIN 取反溢出。
    if val < 0 {
        return format!("-{}", format_thousands(val.unsigned_abs()));
    }
    format_thousands(val as u64)
}

/// 将非负整数按千分位插入逗号分组。
fn format_thousands(val: u64) -> String {
    let s = val.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

fn format_dt(dt: &DateTime<Utc>) -> String {
    dt.format("%m-%d %H:%M").to_string()
}
