use std::sync::atomic::{AtomicU64, Ordering};

use dioxus::prelude::*;
use dioxus_icons::lucide::{Eye, EyeOff};

static NEXT_INPUT_ID: AtomicU64 = AtomicU64::new(1);

/// TextInput —— 表单输入框。
///
/// 按 DESIGN.md §3.6 规格。`dense: true` 切换为 Modal 内的紧凑变体。
#[component]
pub fn TextInput(
    value: Signal<String>,
    #[props(default)] label: Option<String>,
    #[props(default)] placeholder: Option<String>,
    #[props(default = InputType::Text)] input_type: InputType,
    #[props(default = false)] required: bool,
    #[props(default = false)] dense: bool,
    #[props(default)] error: Option<String>,
    #[props(default)] hint: Option<String>,
    #[props(default = false)] disabled: bool,
    /// Explicit DOM id. When omitted, the component generates a stable id for
    /// its mounted lifetime so the visible label can target the input.
    #[props(default)]
    id: Option<String>,
    #[props(default)] name: Option<String>,
    #[props(default)] autocomplete: Option<String>,
    #[props(default)] min: Option<String>,
    #[props(default)] max: Option<String>,
    #[props(default)] step: Option<String>,
) -> Element {
    let mut show_password = use_signal(|| false);
    let input_number = use_hook(|| NEXT_INPUT_ID.fetch_add(1, Ordering::Relaxed));
    let input_id = id.unwrap_or_else(|| format!("ains-input-{input_number}"));
    let message_id = format!("{input_id}-message");
    let described_by = if error.is_some() || hint.is_some() {
        message_id.as_str()
    } else {
        ""
    };

    let input_class = if dense {
        "ains-input__field ains-input__field--dense"
    } else {
        "ains-input__field"
    };
    let type_attr = if input_type == InputType::Password && *show_password.read() {
        "text"
    } else {
        match input_type {
            InputType::Text => "text",
            InputType::Email => "email",
            InputType::Password => "password",
            InputType::Number => "number",
        }
    };

    let field_class = if input_type == InputType::Password {
        format!("{} ains-input__field--pw", input_class)
    } else {
        input_class.to_string()
    };

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/text_input.css") }
        div { class: "ains-input",
            if let Some(label_text) = label.as_ref() {
                label { class: "ains-input__label", r#for: input_id.as_str(), "{label_text}" }
            }
            div { class: "ains-input__field-wrapper",
                input {
                    id: input_id.as_str(),
                    class: "{field_class}",
                    r#type: type_attr,
                    value,
                    required,
                    disabled,
                    placeholder: placeholder.unwrap_or_default(),
                    name: name.unwrap_or_default(),
                    autocomplete: autocomplete.unwrap_or_default(),
                    min: min.unwrap_or_default(),
                    max: max.unwrap_or_default(),
                    step: step.unwrap_or_default(),
                    aria_invalid: if error.is_some() { "true" } else { "false" },
                    aria_describedby: described_by,
                    oninput: move |e| {
                        *value.write() = e.value();
                    },
                }
                if input_type == InputType::Password {
                    button {
                        class: "ains-input__toggle-pw",
                        r#type: "button",
                        tabindex: "-1",
                        aria_label: if *show_password.read() { "Hide password" } else { "Show password" },
                        onclick: move |_| {
                            let current = *show_password.read();
                            *show_password.write() = !current;
                        },
                        if *show_password.read() {
                            Eye { width: "18", height: "18" }
                        } else {
                            EyeOff { width: "18", height: "18" }
                        }
                    }
                }
            }
            if let Some(err) = error.as_ref() {
                p { id: message_id.as_str(), class: "ains-input__error", "{err}" }
            } else if let Some(hint_text) = hint.as_ref() {
                p { id: message_id.as_str(), class: "ains-input__hint", "{hint_text}" }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputType {
    #[default]
    Text,
    Email,
    Password,
    Number,
}

#[cfg(test)]
mod tests {
    use dioxus::dioxus_core::{AttributeValue, Mutation, VirtualDom};

    use super::*;

    fn constrained_number_input_test_app() -> Element {
        let value = use_signal(String::new);
        rsx! {
            TextInput {
                value,
                id: Some("purchase-limit".to_string()),
                label: Some("Purchase limit".to_string()),
                input_type: InputType::Number,
                required: true,
                min: Some("1".to_string()),
                max: Some("36500".to_string()),
                step: Some("1".to_string()),
                error: Some("Invalid limit".to_string()),
            }
        }
    }

    fn has_text_attribute(mutations: &[Mutation], name: &str, expected: &str) -> bool {
        mutations.iter().any(|mutation| {
            matches!(
                mutation,
                Mutation::SetAttribute {
                    name: actual_name,
                    value: AttributeValue::Text(value),
                    ..
                } if *actual_name == name && value == expected
            )
        })
    }

    #[test]
    fn constrained_number_input_renders_label_and_validation_contract() {
        let mut dom = VirtualDom::new(constrained_number_input_test_app);
        let mutations = dom.rebuild_to_vec();
        let edits = mutations.edits.as_slice();

        for (name, value) in [
            ("for", "purchase-limit"),
            ("id", "purchase-limit"),
            ("type", "number"),
            ("min", "1"),
            ("max", "36500"),
            ("step", "1"),
            ("aria-invalid", "true"),
            ("aria-describedby", "purchase-limit-message"),
            ("id", "purchase-limit-message"),
        ] {
            assert!(
                has_text_attribute(edits, name, value),
                "missing rendered {name}={value:?}: {edits:?}"
            );
        }
        assert!(edits.iter().any(|mutation| matches!(
            mutation,
            Mutation::SetAttribute {
                name: "required",
                value: AttributeValue::Bool(true),
                ..
            }
        )));
        assert!(edits.iter().any(|mutation| matches!(
            mutation,
            Mutation::CreateTextNode { value, .. } if value == "Invalid limit"
        )));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::HtmlInputElement;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn non_negative_number_input_accepts_zero_and_ten_decimal_places() {
        let document = web_sys::window()
            .expect("browser window")
            .document()
            .expect("browser document");
        let input = document
            .create_element("input")
            .expect("create number input")
            .dyn_into::<HtmlInputElement>()
            .expect("HTML input element");
        input.set_type("number");
        input.set_min("0");
        input.set_step("any");

        input.set_value("10.0000000001");
        assert!(
            input.check_validity(),
            "the browser must accept the precision supported by stored prices"
        );

        input.set_value("0");
        assert!(
            input.check_validity(),
            "free plans must satisfy the browser's minimum-price constraint"
        );

        input.set_value("-0.0000000001");
        assert!(
            !input.check_validity(),
            "negative prices must remain below the minimum"
        );
    }
}
