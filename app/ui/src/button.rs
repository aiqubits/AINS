use dioxus::prelude::*;
use dioxus_icons::lucide::LoaderCircle;

/// 按钮 —— 唯一 CTA 变体（indigo→purple 渐变）。
///
/// 按 DESIGN.md §3.4 规格实现。原系统中**不**提供 secondary / text / icon 变体。
#[component]
pub fn Button(
    onclick: Option<EventHandler<MouseEvent>>,
    children: Element,
    #[props(default = false)] disabled: bool,
    #[props(default = ButtonType::Button)] button_type: ButtonType,
    #[props(default = false)] full_width: bool,
    #[props(default = false)] loading: bool,
) -> Element {
    let class = button_class(full_width, button_type);

    let type_attr = match button_type {
        ButtonType::Button => "button",
        ButtonType::Submit => "submit",
        ButtonType::Danger => "button",
    };

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/button.css") }
        button {
            class,
            r#type: type_attr,
            disabled: disabled || loading,
            onclick: move |e| {
                if let Some(cb) = onclick {
                    cb.call(e);
                }
            },
            if loading {
                LoaderCircle { class: "ains-btn__spinner" }
            }
            {children}
        }
    }
}

fn button_class(full_width: bool, button_type: ButtonType) -> &'static str {
    match (full_width, button_type) {
        (true, ButtonType::Danger) => "ains-btn ains-btn--danger ains-btn--block",
        (false, ButtonType::Danger) => "ains-btn ains-btn--danger",
        (true, _) => "ains-btn ains-btn--primary ains-btn--block",
        (false, _) => "ains-btn ains-btn--primary",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonType {
    #[default]
    Button,
    Submit,
    /// Danger-style button for destructive actions (e.g., "log out all devices").
    /// Renders as a regular HTML button; the red gradient is provided via
    /// `button.css` (`.ains-btn--danger`, shared across all platforms).
    Danger,
}

#[cfg(test)]
mod tests {
    use dioxus::dioxus_core::{AttributeValue, Mutation, VirtualDom};
    use dioxus::prelude::*;

    use super::Button;
    use super::{ButtonType, button_class};

    fn submit_button_test_app() -> Element {
        rsx! {
            Button {
                onclick: None,
                button_type: ButtonType::Submit,
                "Submit"
            }
        }
    }

    #[test]
    fn button_variants_include_the_base_class_and_expected_variant() {
        assert_eq!(
            button_class(false, ButtonType::Button),
            "ains-btn ains-btn--primary"
        );
        assert_eq!(
            button_class(true, ButtonType::Submit),
            "ains-btn ains-btn--primary ains-btn--block"
        );
        assert_eq!(
            button_class(false, ButtonType::Danger),
            "ains-btn ains-btn--danger"
        );
        assert_eq!(
            button_class(true, ButtonType::Danger),
            "ains-btn ains-btn--danger ains-btn--block"
        );
    }

    #[test]
    fn submit_variant_renders_native_submit_semantics() {
        let mut dom = VirtualDom::new(submit_button_test_app);
        let mutations = dom.rebuild_to_vec();

        assert!(mutations.edits.iter().any(|mutation| matches!(
            mutation,
            Mutation::SetAttribute {
                name: "type",
                value: AttributeValue::Text(value),
                ..
            } if value == "submit"
        )));
    }
}
