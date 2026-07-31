use dioxus::prelude::*;

use crate::{EN, I18nContext, Modal};

/// 权限确认请求的展示模型（宿主从 `PermissionRequest` 映射并**预先脱敏**，
/// 见 AINS_PLAN 6.11：token/password 等敏感字段须在传入前掩码）。
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequestView {
    pub tool_name: String,
    pub reason: String,
    /// 权限引擎实际求值的规范化路径（若有）。
    pub resolved_file_path: Option<String>,
    /// 权限引擎实际求值的命令（若有）。
    pub command: Option<String>,
    /// 脱敏后的调用参数 JSON（pretty 格式）。
    pub input_preview: String,
}

/// 用户对权限请求的三态答复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionChoice {
    Allow,
    AlwaysAllow,
    Deny,
}

/// 权限确认弹窗 —— 允许 / 总是允许 / 拒绝（Phase 6.11）。
///
/// 关闭按钮 / 背板点击等同拒绝（fail-closed）。
#[component]
pub fn PermissionDialog(
    request: PermissionRequestView,
    on_choice: EventHandler<PermissionChoice>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/permission.css") }
        Modal {
            title: t.perm_dialog_title.to_string(),
            on_close: move |_| on_choice.call(PermissionChoice::Deny),
            disable_backdrop: true,
            hide_close: true,
            div { class: "ains-perm__body",
                div { class: "ains-perm__field",
                    span { class: "ains-perm__label", {t.perm_dialog_tool_label} }
                    code { class: "ains-perm__value ains-perm__value--tool", "{request.tool_name}" }
                }
                div { class: "ains-perm__field",
                    span { class: "ains-perm__label", {t.perm_dialog_reason_label} }
                    span { class: "ains-perm__value", "{request.reason}" }
                }
                if let Some(path) = &request.resolved_file_path {
                    div { class: "ains-perm__field",
                        span { class: "ains-perm__label", {t.perm_dialog_path_label} }
                        code { class: "ains-perm__value", "{path}" }
                    }
                }
                if let Some(command) = &request.command {
                    div { class: "ains-perm__field",
                        span { class: "ains-perm__label", {t.perm_dialog_command_label} }
                        code { class: "ains-perm__value", "{command}" }
                    }
                }
                if !request.input_preview.is_empty() {
                    div { class: "ains-perm__field ains-perm__field--block",
                        span { class: "ains-perm__label", {t.perm_dialog_input_label} }
                        pre { class: "ains-perm__pre", "{request.input_preview}" }
                    }
                }
                div { class: "ains-perm__actions",
                    button {
                        class: "ains-perm__btn ains-perm__btn--deny",
                        r#type: "button",
                        onclick: move |_| on_choice.call(PermissionChoice::Deny),
                        {t.perm_deny}
                    }
                    button {
                        class: "ains-perm__btn ains-perm__btn--always",
                        r#type: "button",
                        onclick: move |_| on_choice.call(PermissionChoice::AlwaysAllow),
                        {t.perm_always_allow}
                    }
                    button {
                        class: "ains-perm__btn ains-perm__btn--allow",
                        r#type: "button",
                        onclick: move |_| on_choice.call(PermissionChoice::Allow),
                        {t.perm_allow}
                    }
                }
            }
        }
    }
}
