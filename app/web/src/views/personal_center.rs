//! 个人中心视图（所有角色可访问）。
//!
//! 用于个人账户的套餐购买、账户余额充值、账单记录等功能。
//! 当前后端尚未提供对应能力，先做空占位展示，待后端接口就绪后接入。

use dioxus::prelude::*;
use dioxus_icons::lucide::Wallet;

use ui::I18nContext;

#[component]
pub fn PersonalCenter() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();

    rsx! {
        div { class: "ains-users",
            header { class: "ains-users__header",
                div { class: "ains-users__title-block",
                    h1 { class: "ains-users__title", "{t.personal_center_title}" }
                    p { class: "ains-users__subtitle", "{t.personal_center_subtitle}" }
                }
            }

            // 空占位：功能建设中
            div { class: "ains-users__status",
                Wallet {}
                "{t.personal_center_empty}"
            }
        }
    }
}
