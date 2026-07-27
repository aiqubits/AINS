//! 共享「所属租户」自绘下拉（用户管理与套餐管理共用）。
//!
//! 原生 `<select>` 弹层无法与 Modal 风格对齐，故自绘触发器 + 弹层；
//! 选项仅展示 `active` 租户，文案统一为 `名称 (短ID...)`，并支持「无限
//! 滚动」分页加载。抽取到此模块前，users.rs 与 plans.rs 各存一份近乎
//! 相同的实现（约 130 行），极易在后续维护中漂移。
//!
//! 关键不变量：
//! - 分页读取一律使用 `peek()` 而非 `read()`，避免在 `use_effect` 中同步
//!   读取信号造成额外订阅、进而无限重入（详见 [`load_tenant_page`]）。
//! - 渲染期读取（当前选择/展开态/列表）使用 `read()`，以便选择变化时
//!   触发重渲染。

use client_api::{Client, TenantResponse};
use dioxus::prelude::dioxus_router::Navigator;
use dioxus::prelude::*;
use dioxus_icons::lucide::ChevronDown;

use crate::auth::AuthState;
use crate::components::{HttpMethod, LogBus, push_log_err};

use super::{element_near_bottom, short_tenant_id};

/// 是否还有下一页。抽为纯函数便于对边界（`total_pages == 0`、
/// `page == total_pages`）做单元测试，且三处调用点共享同一语义。
pub(crate) fn has_more_pages(page: u64, total_pages: u64) -> bool {
    page < total_pages
}

/// 计算触发器展示文案：
/// - 选中项存在（可为任意状态，含已停用）→ `名称 (短ID...)`；
/// - 未选择（空串）→ `empty_placeholder`（各视图语义不同：用户管理为
///   「—」，套餐管理为「请选择所属租户」）；
/// - 选中了列表中不存在的 ID → 回退展示其短 ID。
pub(crate) fn selected_tenant_label(
    tenants: &[TenantResponse],
    selected_id: &str,
    empty_placeholder: &str,
) -> String {
    tenants
        .iter()
        .find(|t| t.id == selected_id)
        .map(|t| format!("{} ({})", t.name, short_tenant_id(&t.id)))
        .unwrap_or_else(|| {
            if selected_id.is_empty() {
                empty_placeholder.to_string()
            } else {
                short_tenant_id(selected_id)
            }
        })
}

/// 拉取「下一页」租户（页码取自 `next_page` 信号，初始为 1，故本函数
/// 同时服务于首屏加载、下拉滚动加载与失败重试）。
///
/// 防停滞：客户端按 `status == "active"` 过滤，而服务端分页覆盖全部
/// 状态。若某一页全是已停用租户，则可见选项不增长、面板高度不变、
/// `onscroll` 不再触发，后续页将永久不可达。故当本页未新增任何 active
/// 选项且仍有更多页时，在同一异步任务内自动续拉下一页，直到出现可见
/// 选项或翻完所有页（以 `total_pages` 为界，必然终止）。
///
/// 所有响应式读取均用 `peek()`：本函数会被 `use_effect` 同步调用，若用
/// `read()` 会订阅 loading/next_page 等信号，导致 effect 反复重入。
#[allow(clippy::too_many_arguments)]
pub(crate) fn load_tenant_page(
    client: Client,
    auth: AuthState,
    nav: Navigator,
    log_bus: LogBus,
    mut available: Signal<Vec<TenantResponse>>,
    mut next_page: Signal<u64>,
    mut has_more: Signal<bool>,
    mut loading: Signal<bool>,
) {
    // 同步去重：并发的 onscroll / 重试不会触发重复拉取。
    if *loading.peek() {
        return;
    }
    loading.set(true);
    spawn(async move {
        loop {
            let page = *next_page.peek();
            match client.list_tenants(page, 100).await {
                Ok(data) => {
                    let more = has_more_pages(data.page, data.total_pages);
                    let added_active = data.items.iter().any(|t| t.status == "active");
                    available.with_mut(|v| v.extend(data.items));
                    next_page.set(page + 1);
                    has_more.set(more);
                    // 本页有可见选项，或已无更多页 → 结束；否则续拉防停滞。
                    if added_active || !more {
                        loading.set(false);
                        break;
                    }
                }
                Err(err) => {
                    if crate::api::handle_unauth(&err, auth.clone(), nav, log_bus).await {
                        return;
                    }
                    push_log_err(log_bus, HttpMethod::Get, "/api/tenants", &err);
                    loading.set(false);
                    break;
                }
            }
        }
    });
}

/// [`render_tenant_select`] 的入参集合（信号 + 上下文 + 展示配置）。
#[derive(Clone)]
pub(crate) struct TenantSelectView {
    /// 字段标签文案。
    pub label: String,
    /// 弹层滚动容器的 DOM id（各实例必须唯一，供 `element_near_bottom` 定位）。
    pub panel_id: &'static str,
    /// 是否禁用（提交中）。
    pub disabled: bool,
    /// 是否向下展开（字段位于 Modal 顶部时用 `--down`，避免 drop-up 越出被裁切）。
    pub drop_down: bool,
    /// 未选择时触发器展示的占位文案。
    pub empty_placeholder: String,
    pub available_tenants: Signal<Vec<TenantResponse>>,
    /// 当前选中的租户 ID（即各视图的 `form_tenant_id`）。
    pub selected_id: Signal<String>,
    /// 下拉是否展开（即各视图的 `tenant_dropdown_open`）。
    pub open: Signal<bool>,
    pub next_page: Signal<u64>,
    pub has_more: Signal<bool>,
    pub loading: Signal<bool>,
    pub client: Client,
    pub auth: AuthState,
    pub nav: Navigator,
    pub log_bus: LogBus,
}

/// 渲染「所属租户」下拉字段（`div.ains-input > label + div.ains-select`）。
/// 调用方负责外层可见性条件（如 system-only / create-only）与外层点击空白关闭。
pub(crate) fn render_tenant_select(view: TenantSelectView) -> Element {
    let TenantSelectView {
        label,
        panel_id,
        disabled,
        drop_down,
        empty_placeholder,
        available_tenants,
        selected_id,
        open,
        next_page,
        has_more,
        loading,
        client,
        auth,
        nav,
        log_bus,
    } = view;

    // 渲染期读取（订阅，以便选择/展开/列表变化时重渲染）。
    let tenants_snapshot = available_tenants.read().clone();
    let selected_id_val = selected_id.read().clone();
    let open_val = *open.read();
    let label_text = selected_tenant_label(&tenants_snapshot, &selected_id_val, &empty_placeholder);
    let active_tenants: Vec<TenantResponse> = tenants_snapshot
        .into_iter()
        .filter(|t| t.status == "active")
        .collect();

    let select_class = match (drop_down, open_val) {
        (true, true) => "ains-select ains-select--down ains-select--open",
        (true, false) => "ains-select ains-select--down",
        (false, true) => "ains-select ains-select--open",
        (false, false) => "ains-select",
    };

    // 触发器点击：切换展开态；若因首屏拉取失败导致列表仍为空
    // （next_page 未推进过 1），在打开时重试首页，避免永久空下拉。
    let trigger_client = client.clone();
    let trigger_auth = auth.clone();
    let mut open_sig = open;
    let on_trigger = move |_: MouseEvent| {
        let cur = *open_sig.peek();
        open_sig.set(!cur);
        if !cur && *next_page.peek() == 1 && available_tenants.peek().is_empty() && !*loading.peek()
        {
            load_tenant_page(
                trigger_client.clone(),
                trigger_auth.clone(),
                nav,
                log_bus,
                available_tenants,
                next_page,
                has_more,
                loading,
            );
        }
    };

    // 滚动到接近底部时加载下一页（仍有更多且未在加载时）。
    let scroll_client = client.clone();
    let scroll_auth = auth.clone();

    rsx! {
        div { class: "ains-input",
            label { class: "ains-input__label", "{label}" }
            div {
                class: "{select_class}",
                // 阻止下拉内部点击冒泡到外层「点击空白关闭」逻辑。
                onclick: move |e: MouseEvent| e.stop_propagation(),
                button {
                    r#type: "button",
                    class: "ains-select__trigger",
                    disabled,
                    onclick: on_trigger,
                    span { class: "ains-select__value", "{label_text}" }
                    ChevronDown { class: "ains-select__chevron" }
                }
                if open_val {
                    div {
                        class: "ains-select__panel",
                        id: "{panel_id}",
                        onscroll: move |_| {
                            if element_near_bottom(panel_id) && *has_more.peek() && !*loading.peek() {
                                load_tenant_page(
                                    scroll_client.clone(),
                                    scroll_auth.clone(),
                                    nav,
                                    log_bus,
                                    available_tenants,
                                    next_page,
                                    has_more,
                                    loading,
                                );
                            }
                        },
                        for tenant in active_tenants.iter() {
                            {
                                let tid = tenant.id.clone();
                                let is_sel = tenant.id == selected_id_val;
                                let opt_label =
                                    format!("{} ({})", tenant.name, short_tenant_id(&tenant.id));
                                let mut tid_sig = selected_id;
                                let mut close_sig = open;
                                rsx! {
                                    button {
                                        key: "{tenant.id}",
                                        r#type: "button",
                                        class: if is_sel { "ains-select__option ains-select__option--active" } else { "ains-select__option" },
                                        onclick: move |_: MouseEvent| {
                                            tid_sig.set(tid.clone());
                                            close_sig.set(false);
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
}

#[cfg(test)]
mod tests {
    use super::{has_more_pages, selected_tenant_label};
    use chrono::Utc;
    use client_api::TenantResponse;

    fn tenant(id: &str, name: &str, status: &str) -> TenantResponse {
        TenantResponse {
            id: id.to_string(),
            name: name.to_string(),
            status: status.to_string(),
            user_count: 0,
            channel_count: 0,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn has_more_pages_boundaries() {
        // 空结果：total_pages == 0 → 无更多
        assert!(!has_more_pages(1, 0));
        // 最后一页：page == total_pages → 无更多
        assert!(!has_more_pages(3, 3));
        // 中间页：仍有更多
        assert!(has_more_pages(1, 3));
        assert!(has_more_pages(2, 3));
        // 越界保护：page 超过 total_pages 也判定为无更多
        assert!(!has_more_pages(4, 3));
    }

    #[test]
    fn label_uses_placeholder_when_unselected() {
        let tenants = vec![tenant("t1", "Alpha", "active")];
        assert_eq!(
            selected_tenant_label(&tenants, "", "请选择所属租户"),
            "请选择所属租户"
        );
        assert_eq!(selected_tenant_label(&[], "", "—"), "—");
    }

    #[test]
    fn label_formats_selected_tenant_with_short_id() {
        let tenants = vec![
            tenant("1785041947612751483", "Alpha", "active"),
            tenant("short", "Beta", "disabled"),
        ];
        // 长 ID 截断为前 8 位 + 省略号
        assert_eq!(
            selected_tenant_label(&tenants, "1785041947612751483", "—"),
            "Alpha (17850419...)"
        );
        // 选中的即使是已停用租户也应正确展示名称（触发器不受 active 过滤限制）
        assert_eq!(
            selected_tenant_label(&tenants, "short", "—"),
            "Beta (short)"
        );
    }

    #[test]
    fn label_falls_back_to_short_id_when_selection_missing() {
        // 选中了列表中不存在的 ID（如列表尚未加载到该页）→ 回退展示短 ID
        let tenants = vec![tenant("t1", "Alpha", "active")];
        assert_eq!(
            selected_tenant_label(&tenants, "1785041947612751483", "—"),
            "17850419..."
        );
    }
}
