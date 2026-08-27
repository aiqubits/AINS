use crate::translate;

// All translations loaded from external data file.
// Add new entries in translate_fields.rs — no changes needed here.
include!("translate_fields.rs");

// ============================================================
// Compile-time cross-validation tests
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Language;

    /// 品牌名 / 技术术语豁免列表——这些字段 EN=ZH 是正常预期，不触发断言。
    const EXEMPT: &[&str] = &[
        "sidebar_brand_title",               // 品牌名 AINS 不翻译
        "top_header_node_online",            // 技术术语 Node Online 不翻译
        "dashboard_middleware_active",       // 技术术语 Active 不翻译
        "auth_name_placeholder",             // 占位符 e.g., rust_master 中英相同
        "auth_email_placeholder_login",      // admin@ains.local
        "auth_email_placeholder_register",   // admin@ains.local
        "auth_password_placeholder",         // 12345
        "app_shell_guest",                   // Guest 品牌名不翻译
        "users_form_name_placeholder",       // 占位符 e.g., rust_master 中英相同
        "users_form_email_placeholder",      // 占位符 master@rust.org 中英相同
        "channels_form_protocol_anthropic",  // 品牌名 Anthropic 不翻译
        "channels_form_api_key_label",       // 技术术语 API Key 不翻译
        "channels_form_api_key_placeholder", // 占位符 sk-… 不翻译
        "metering_column_prompt_tokens",     // 技术术语 Prompt 不翻译
        "metering_column_completion_tokens", // 技术术语 Completion 不翻译
    ];

    /// 防止 EN/ZH 倒挂粘贴错误。
    ///
    /// 自动遍历 translate! 宏生成的 ALL_TRANSLATION_FIELDS，新增字段无需手动枚举。
    /// 品牌名 / 技术术语通过 EXEMPT 列表跳过。
    #[test]
    fn en_zh_values_not_identical() {
        for (name, en_val, zh_val) in ALL_TRANSLATION_FIELDS {
            if EXEMPT.contains(name) {
                continue;
            }
            assert!(
                en_val != zh_val,
                "EN/ZH 值相同可能为倒挂粘贴错误: {} (EN=`{}`, ZH=`{zh_val}`) — 若确属有意为之（如占位符/技术术语），请加入 EXEMPT 列表",
                name,
                en_val,
            );
        }
    }

    /// 防止漏译：ZH 必须非空。EN 允许为空（如 `users_per_page_unit: "" => "条"`
    /// 这类英文无单位的占位字段），但 ZH 漏填会直接导致页面显示空白。
    #[test]
    fn zh_values_not_empty() {
        for (name, _en_val, zh_val) in ALL_TRANSLATION_FIELDS {
            assert!(
                !zh_val.is_empty(),
                "ZH 翻译为空: {name} — 漏译会导致页面显示空白，请补齐翻译"
            );
        }
    }

    /// as_str() 覆盖所有 Language 变体
    #[test]
    fn language_as_str_covers_all_variants() {
        assert_eq!(Language::En.as_str(), "en");
        assert_eq!(Language::Zh.as_str(), "zh");
    }

    /// 确保 ALL_TRANSLATION_FIELDS 数组行数与 translate! 调用中的字段数一致。
    /// 这是一个防遗漏守卫——当新增或删除字段时，此计数值需同步更新。
    #[test]
    fn all_translation_fields_count() {
        let count = ALL_TRANSLATION_FIELDS.len();
        assert_eq!(
            count, 621,
            "ALL_TRANSLATION_FIELDS 计数 ({count}) 不符合预期 (621)。如果新增/删除了 translate! 字段，请同步更新此断言。"
        );
    }

    #[test]
    fn management_pagination_translations_use_module_specific_entities() {
        let info_args: &[(&str, &dyn std::fmt::Display)] =
            &[("total", &1), ("page", &1), ("total_pages", &1)];
        let simple_args: &[(&str, &dyn std::fmt::Display)] = &[("total", &0)];

        for (info, simple, expected_info, expected_simple) in [
            (
                EN.tenants_count_info,
                EN.tenants_count_simple,
                "Tenants: 1, page 1 / 1",
                "Tenants: 0",
            ),
            (
                EN.channels_count_info,
                EN.channels_count_simple,
                "Channels: 1, page 1 / 1",
                "Channels: 0",
            ),
            (
                EN.plans_count_info,
                EN.plans_count_simple,
                "Plans: 1, page 1 / 1",
                "Plans: 0",
            ),
            (
                EN.orders_count_info,
                EN.orders_count_simple,
                "Orders: 1, page 1 / 1",
                "Orders: 0",
            ),
            (
                ZH.tenants_count_info,
                ZH.tenants_count_simple,
                "共 1 个租户，第 1 / 1 页",
                "共 0 个租户",
            ),
            (
                ZH.channels_count_info,
                ZH.channels_count_simple,
                "共 1 个渠道，第 1 / 1 页",
                "共 0 个渠道",
            ),
            (
                ZH.plans_count_info,
                ZH.plans_count_simple,
                "共 1 个套餐，第 1 / 1 页",
                "共 0 个套餐",
            ),
            (
                ZH.orders_count_info,
                ZH.orders_count_simple,
                "共 1 个订单，第 1 / 1 页",
                "共 0 个订单",
            ),
        ] {
            assert_eq!(crate::tf(info, info_args), expected_info);
            assert_eq!(crate::tf(simple, simple_args), expected_simple);
        }
    }
}
