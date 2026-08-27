use dioxus::prelude::*;

/// AppShell —— 根布局。
///
/// 按 DESIGN.md §3.1 规格：flex + min-height:100vh，应用 page-gradient 背景。
///
/// 注意：设计令牌 `tokens.css` 由 `ui::GlobalStyles` 在根 App 统一注入，
/// 本组件不再重复加载。消费方必须确保根 App 挂载了 `ui::GlobalStyles {}`。
#[component]
pub fn AppShell(
    sidebar: Element,
    top_header: Element,
    children: Element,
    /// Route-level modifier for the main viewport container.
    ///
    /// The chat view supplies this so the shell itself cannot become a second
    /// vertical scroll container beside the message list.
    #[props(default)]
    main_class: String,
    /// Route-level modifier for content width and views that manage their own scrolling.
    #[props(default)]
    content_class: String,
) -> Element {
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/app_shell.css") }
        div { class: "ains-app-shell",
            {sidebar}
            div { class: "ains-app-shell__main {main_class}",
                {top_header}
                main { class: "ains-app-shell__content {content_class}", {children} }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use dioxus::dioxus_core::{AttributeValue, Mutation, ScopeId, VirtualDom};

    use super::*;

    #[derive(Clone, Copy)]
    enum TestLayout {
        Default,
        Wide,
        Chat,
    }

    #[derive(Clone, Props)]
    struct AppShellTransitionProps {
        layout: Rc<Cell<TestLayout>>,
    }

    impl PartialEq for AppShellTransitionProps {
        fn eq(&self, other: &Self) -> bool {
            Rc::ptr_eq(&self.layout, &other.layout)
        }
    }

    fn app_shell_transition_test_app(props: AppShellTransitionProps) -> Element {
        let (main_class, content_class) = match props.layout.get() {
            TestLayout::Default => ("", ""),
            TestLayout::Wide => ("", "ains-app-shell__content--wide-management"),
            TestLayout::Chat => (
                "ains-app-shell__main--chat",
                "ains-app-shell__content--chat",
            ),
        };

        rsx! {
            AppShell {
                sidebar: rsx! { nav { "Sidebar" } },
                top_header: rsx! { header { "Header" } },
                main_class: main_class.to_string(),
                content_class: content_class.to_string(),
                div { "Content" }
            }
        }
    }

    fn has_exact_class_tokens(mutations: &[Mutation], expected: &[&str]) -> bool {
        mutations.iter().any(|mutation| {
            matches!(
                mutation,
                Mutation::SetAttribute {
                    name: "class",
                    value: AttributeValue::Text(value),
                    ..
                } if value.split_whitespace().eq(expected.iter().copied())
            )
        })
    }

    #[test]
    fn route_layout_prop_transitions_replace_shell_modifiers_without_stale_classes() {
        let layout = Rc::new(Cell::new(TestLayout::Default));
        let mut dom = VirtualDom::new_with_props(
            app_shell_transition_test_app,
            AppShellTransitionProps {
                layout: layout.clone(),
            },
        );

        let initial = dom.rebuild_to_vec();
        assert!(has_exact_class_tokens(
            &initial.edits,
            &["ains-app-shell__content"]
        ));

        layout.set(TestLayout::Wide);
        dom.mark_dirty(ScopeId::APP);
        let wide = dom.render_immediate_to_vec();
        assert!(has_exact_class_tokens(
            &wide.edits,
            &[
                "ains-app-shell__content",
                "ains-app-shell__content--wide-management"
            ]
        ));

        layout.set(TestLayout::Chat);
        dom.mark_dirty(ScopeId::APP);
        let chat = dom.render_immediate_to_vec();
        assert!(has_exact_class_tokens(
            &chat.edits,
            &["ains-app-shell__main", "ains-app-shell__main--chat"]
        ));
        assert!(has_exact_class_tokens(
            &chat.edits,
            &["ains-app-shell__content", "ains-app-shell__content--chat"]
        ));

        layout.set(TestLayout::Default);
        dom.mark_dirty(ScopeId::APP);
        let reset = dom.render_immediate_to_vec();
        assert!(has_exact_class_tokens(
            &reset.edits,
            &["ains-app-shell__main"]
        ));
        assert!(has_exact_class_tokens(
            &reset.edits,
            &["ains-app-shell__content"]
        ));
    }
}
