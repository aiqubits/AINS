mod agent_chat;
mod auth;
mod channels;
mod dashboard;
mod forgot_password;
mod login_landing;
mod memory;
mod metering;
mod not_found;
mod orders;
mod personal_center;
mod plans;
mod reset_password;
mod settings;
mod skills;
mod tenant_select;
mod tenants;
mod tools;
mod users;
mod verify_email;

use ui::{Translations, tf};

pub use agent_chat::AgentChat;
pub use auth::Auth;
pub use channels::Channels;
pub use dashboard::Dashboard;
pub use forgot_password::ForgotPassword;
pub use login_landing::LoginLanding;
pub use memory::Memory;
pub use metering::Metering;
pub use not_found::NotFound;
pub use orders::Orders;
pub use personal_center::PersonalCenter;
pub use plans::Plans;
pub use reset_password::ResetPassword;
pub use settings::Settings;
pub use skills::Skills;
pub use tenants::Tenants;
pub use tools::Tools;
pub use users::Users;
pub use verify_email::VerifyEmail;

/// Returns true when the scroll container identified by `id` is scrolled to
/// (or near) its bottom edge. Used by the infinite-scroll tenant dropdowns to
/// decide when to fetch the next page. Returns false when the element or the
/// browser globals are unavailable (e.g. during SSR/hydration or host tests).
#[cfg(target_arch = "wasm32")]
pub(crate) fn element_near_bottom(id: &str) -> bool {
    const THRESHOLD_PX: f64 = 48.0;
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    else {
        return false;
    };
    let scroll_top = el.scroll_top() as f64;
    let client_height = el.client_height() as f64;
    let scroll_height = el.scroll_height() as f64;
    scroll_top + client_height >= scroll_height - THRESHOLD_PX
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn element_near_bottom(_id: &str) -> bool {
    false
}

/// 截断租户 ID 仅展示前 8 位（超过 8 位附省略号），用于自绘租户下拉的
/// 选项与触发器文案（用户管理与套餐管理共用，保证两处展示完全对齐）。
pub(crate) fn short_tenant_id(id: &str) -> String {
    if id.chars().count() > 8 {
        let head: String = id.chars().take(8).collect();
        format!("{head}...")
    } else {
        id.to_string()
    }
}

/// 订单状态 → 本地化文案（订单管理页与个人中心账单共用）。
///
/// 未知状态原样透出（中性回退）—— 账单/订单数据以诚实展示为先，
/// 不得把未知状态渲染成“待支付”造成误导；与套餐状态的回退策略一致。
pub(crate) fn order_status_label<'a>(t: &'static Translations, status: &'a str) -> &'a str {
    match status {
        "paid" => t.orders_status_paid,
        "pending" => t.orders_status_pending,
        "refunded" => t.orders_status_refunded,
        "cancelled" => t.orders_status_cancelled,
        other => other,
    }
}

/// 支付方式 → 本地化文案（订单管理页与个人中心账单共用）。
/// 未知方式原样透出，理由同 [`order_status_label`]。
pub(crate) fn order_method_label<'a>(t: &'static Translations, method: &'a str) -> &'a str {
    match method {
        "balance" => t.orders_method_balance,
        "wechat" => t.orders_method_wechat,
        "alipay" => t.orders_method_alipay,
        other => other,
    }
}

/// Per-user purchase limit display shared by the management and self-service
/// plan tables. `None` means the plan can be purchased without a count limit.
pub(crate) fn format_purchase_limit(
    t: &'static Translations,
    purchase_limit: Option<i32>,
) -> String {
    match purchase_limit {
        Some(1) => t.plans_purchase_limit_once.to_string(),
        Some(limit) => tf(
            t.plans_purchase_limit_count,
            &[("count", &limit.to_string())],
        ),
        None => t.plans_purchase_limit_unlimited.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{format_purchase_limit, order_method_label, order_status_label, short_tenant_id};
    use i18n::{EN, ZH};

    #[test]
    fn short_tenant_id_truncates_long_ids() {
        assert_eq!(short_tenant_id("1234567890abcdef"), "12345678...");
        // 8 位及以内原样返回
        assert_eq!(short_tenant_id("default"), "default");
        assert_eq!(short_tenant_id("12345678"), "12345678");
    }

    #[test]
    fn short_tenant_id_truncates_by_chars_not_bytes() {
        // 多字节字符按 chars() 而非字节截断，锁定不会在 UTF-8 边界 panic
        assert_eq!(short_tenant_id("一二三四五六七八九"), "一二三四五六七八...");
        assert_eq!(short_tenant_id("一二三四五六七八"), "一二三四五六七八");
    }

    #[test]
    fn order_status_known_values_are_localized() {
        assert_eq!(order_status_label(&EN, "paid"), EN.orders_status_paid);
        assert_eq!(order_status_label(&EN, "pending"), EN.orders_status_pending);
        assert_eq!(
            order_status_label(&EN, "refunded"),
            EN.orders_status_refunded
        );
        assert_eq!(
            order_status_label(&EN, "cancelled"),
            EN.orders_status_cancelled
        );
        // ZH 抽查：守卫 translate! 宏的 EN/ZH 字段映射不错位
        // （辅助函数现在供订单管理页与个人中心两处共用）。
        assert_eq!(order_status_label(&ZH, "paid"), ZH.orders_status_paid);
        assert_ne!(ZH.orders_status_paid, EN.orders_status_paid);
    }

    #[test]
    fn order_status_unknown_passes_through_verbatim() {
        // 未知状态不得回退成“待支付”—— 账单展示必须诚实。
        assert_eq!(order_status_label(&EN, "disputed"), "disputed");
    }

    #[test]
    fn order_method_known_values_are_localized() {
        assert_eq!(order_method_label(&EN, "balance"), EN.orders_method_balance);
        assert_eq!(order_method_label(&EN, "wechat"), EN.orders_method_wechat);
        assert_eq!(order_method_label(&EN, "alipay"), EN.orders_method_alipay);
        // ZH 抽查，理由同上。
        assert_eq!(order_method_label(&ZH, "balance"), ZH.orders_method_balance);
        assert_ne!(ZH.orders_method_balance, EN.orders_method_balance);
    }

    #[test]
    fn order_method_unknown_passes_through_verbatim() {
        assert_eq!(order_method_label(&EN, "stripe"), "stripe");
    }

    #[test]
    fn purchase_limit_display_is_shared_and_localized() {
        assert_eq!(format_purchase_limit(&EN, Some(1)), "1 time");
        assert_eq!(format_purchase_limit(&EN, Some(2)), "2 times");
        assert_eq!(format_purchase_limit(&EN, None), "Unlimited");
        assert_eq!(format_purchase_limit(&ZH, Some(1)), "1 次");
        assert_eq!(format_purchase_limit(&ZH, None), "不限");
    }
}
