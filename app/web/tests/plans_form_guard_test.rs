//! 套餐弹窗与交互原型之间的结构回归守卫。
//!
//! 这里仅约束套餐弹窗确实组合为 form + submit button，以及状态控件
//! 的语义。共享 Button/TextInput 的实际渲染属性由 ui crate 的
//! VirtualDom 单测覆盖，避免在这里重复用源码字符串断言组件内部实现。

const PLANS_VIEW: &str = include_str!("../src/views/plans.rs");

fn form_modal_source() -> &'static str {
    PLANS_VIEW
        .split_once("fn render_form_modal(")
        .expect("render_form_modal must exist")
        .1
        .split_once("fn build_columns(")
        .expect("build_columns must follow render_form_modal")
        .0
}

#[test]
fn plan_modal_submits_through_a_real_form() {
    let source = form_modal_source();

    assert!(source.contains("let on_submit = move |event: FormEvent|"));
    assert!(source.contains("event.prevent_default();"));
    assert!(source.contains("form {"));
    assert!(source.contains("onsubmit: on_submit"));
    assert!(source.contains("button_type: ButtonType::Submit"));
    assert!(source.contains("onclick: None"));
    assert!(!source.contains("let on_submit = move |_: MouseEvent|"));
}

#[test]
fn status_toggle_exposes_group_and_pressed_state() {
    let source = form_modal_source();

    assert!(source.contains("role: \"group\""));
    assert!(source.contains("aria_labelledby: \"plan-status-label\""));
    assert_eq!(source.matches("aria_pressed:").count(), 2);
}

#[test]
fn price_input_keeps_the_ten_decimal_storage_contract() {
    assert!(PLANS_VIEW.contains("const MIN_PLAN_PRICE: &str = \"0\";"));
    assert!(PLANS_VIEW.contains("const PLAN_PRICE_STEP: &str = \"any\";"));
    assert!(form_modal_source().contains("min: Some(MIN_PLAN_PRICE.to_string())"));
    assert!(form_modal_source().contains("step: Some(PLAN_PRICE_STEP.to_string())"));
}
