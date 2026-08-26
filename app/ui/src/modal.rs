use std::sync::atomic::{AtomicU64, Ordering};

use dioxus::dioxus_core::spawn_forever;
use dioxus::prelude::*;
use dioxus_icons::lucide::X;

use crate::{EN, I18nContext};

static NEXT_MODAL_ID: AtomicU64 = AtomicU64::new(1);

const MODAL_MOUNT_SCRIPT: &str = r#"
(() => {
    const modalId = "__AINS_MODAL_ID__";
    const dialog = document.getElementById(modalId);
    if (!dialog) return;
    const backdrop = document.getElementById(`${modalId}-backdrop`);

    const state = window.__ainsModalState ??= {};
    // Keep hot-reload and independently bundled hosts safe if they retained
    // state created by an older Modal implementation.
    state.cleanups ??= {};
    state.inertEntries ??= new Map();
    state.stack ??= [];
    state.previousOverflow ??= "";
    state.focusOrigin ??= null;
    state.cleanups[modalId]?.();

    const previouslyFocused = document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    if (state.stack.length === 0) {
        state.previousOverflow = document.body.style.overflow;
        state.focusOrigin = previouslyFocused;
    }
    state.stack.push(modalId);
    const originalDialogZIndex = dialog.style.zIndex;
    const originalBackdropZIndex = backdrop?.style.zIndex ?? "";
    const syncStack = () => {
        const topIndex = state.stack.length - 1;
        // CSS provides the base layer. Recompute every remaining entry after
        // stack changes so closing a non-top modal cannot leave a gap that a
        // later modal would reuse. Derive dialog interactivity from the same
        // stack so a dialog inerted by an earlier effect becomes interactive
        // when its own mount effect promotes it to the top.
        state.stack.forEach((id, index) => {
            const stackedDialog = document.getElementById(id);
            const stackedBackdrop = document.getElementById(`${id}-backdrop`);
            if (stackedDialog) {
                stackedDialog.style.zIndex = String(70 + index * 20);
                stackedDialog.inert = index !== topIndex;
            }
            if (stackedBackdrop) stackedBackdrop.style.zIndex = String(60 + index * 20);
        });
    };
    const inerted = [];

    let branch = dialog;
    while (branch && branch !== document.body) {
        const parent = branch.parentElement;
        if (!parent) break;
        for (const sibling of parent.children) {
            if (sibling === branch || sibling.classList.contains("ains-modal__backdrop")) continue;
            const entry = state.inertEntries.get(sibling);
            if (entry) {
                entry.count += 1;
            } else {
                state.inertEntries.set(sibling, { count: 1, original: sibling.inert });
            }
            inerted.push(sibling);
            sibling.inert = true;
        }
        branch = parent;
    }
    syncStack();
    document.body.style.overflow = "hidden";

    const focusableElements = () => [...dialog.querySelectorAll(
        'button:not([disabled]):not([hidden]), input:not([disabled]):not([hidden]), select:not([disabled]):not([hidden]), textarea:not([disabled]):not([hidden]), a[href]:not([hidden]), [tabindex]:not([tabindex="-1"]):not([hidden])'
    )].filter((element) => element.getClientRects().length > 0);

    const onKeyDown = (event) => {
        if (state.stack[state.stack.length - 1] !== modalId) return;

        if (event.key === "Escape") {
            event.preventDefault();
            if (dialog.dataset.ainsModalCloseDisabled === "true") return;
            dialog.querySelector('[data-ains-modal-close="true"]')?.click();
            return;
        }
        if (event.key !== "Tab") return;

        const focusable = focusableElements();
        if (focusable.length === 0) {
            event.preventDefault();
            dialog.focus();
            return;
        }

        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        const active = document.activeElement;
        if (event.shiftKey && (active === first || !dialog.contains(active))) {
            event.preventDefault();
            last.focus();
        } else if (!event.shiftKey && (active === last || !dialog.contains(active))) {
            event.preventDefault();
            first.focus();
        }
    };

    document.addEventListener("keydown", onKeyDown);
    queueMicrotask(() => {
        if (!dialog.isConnected || state.stack[state.stack.length - 1] !== modalId) return;
        const initial = dialog.querySelector(
            'input:not([disabled]):not([hidden]), select:not([disabled]):not([hidden]), textarea:not([disabled]):not([hidden])'
        ) ?? focusableElements()[0] ?? dialog;
        initial.focus();
    });

    let cleaned = false;
    state.cleanups[modalId] = () => {
        if (cleaned) return;
        cleaned = true;
        document.removeEventListener("keydown", onKeyDown);
        for (const element of inerted) {
            const entry = state.inertEntries.get(element);
            if (!entry) continue;
            entry.count -= 1;
            if (entry.count === 0) {
                if (element.isConnected) element.inert = entry.original;
                state.inertEntries.delete(element);
            }
        }
        const stackIndex = state.stack.lastIndexOf(modalId);
        if (stackIndex !== -1) state.stack.splice(stackIndex, 1);
        dialog.style.zIndex = originalDialogZIndex;
        if (backdrop) backdrop.style.zIndex = originalBackdropZIndex;
        syncStack();
        const finalFocus = state.stack.length === 0 ? state.focusOrigin : null;
        if (state.stack.length === 0) {
            document.body.style.overflow = state.previousOverflow;
            state.focusOrigin = null;
        }
        delete state.cleanups[modalId];

        queueMicrotask(() => {
            const topModalId = state.stack[state.stack.length - 1];
            const topDialog = topModalId ? document.getElementById(topModalId) : null;
            if (topDialog) {
                if (previouslyFocused?.isConnected && topDialog.contains(previouslyFocused)) {
                    previouslyFocused.focus();
                } else if (!topDialog.contains(document.activeElement)) {
                    const next = topDialog.querySelector(
                        'input:not([disabled]):not([hidden]), select:not([disabled]):not([hidden]), textarea:not([disabled]):not([hidden]), button:not([disabled]):not([hidden]), [tabindex]:not([tabindex="-1"]):not([hidden])'
                    ) ?? topDialog;
                    next.focus();
                }
            } else if (finalFocus?.isConnected) {
                finalFocus.focus();
            }
        });
    };
})();
"#;

const MODAL_CLEANUP_SCRIPT: &str = r#"
(() => {
    const modalId = "__AINS_MODAL_ID__";
    window.__ainsModalState?.cleanups?.[modalId]?.();
})();
"#;

fn modal_script(template: &str, modal_id: &str) -> String {
    template.replace("__AINS_MODAL_ID__", modal_id)
}

/// 通用 Modal 容器 —— 按 DESIGN.md §3.7 规格实现。
///
/// 用法：
/// ```ignore
/// Modal { title: "创建新用户", on_close, open: true,
///     TextInput { label: "账户", value, on_input }
/// }
/// ```
#[component]
pub fn Modal(
    title: String,
    on_close: EventHandler<MouseEvent>,
    #[props(default = true)] open: bool,
    #[props(default = false)] disable_backdrop: bool,
    /// 禁止所有用户触发的关闭入口（遮罩层、Escape 和标题栏关闭按钮）。
    /// 适用于请求提交中，避免旧请求与新弹窗共享状态而产生竞态。
    #[props(default = false)]
    disable_close: bool,
    /// 隐藏标题栏右上角的关闭×（确认类弹窗已有显式取消按钮时，避免冗余）。
    #[props(default = false)]
    hide_close: bool,
    children: Element,
) -> Element {
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/modal.css") }
        if open {
            ModalLayer { title, on_close, disable_backdrop, disable_close, hide_close, children }
        }
    }
}

/// 仅在弹窗打开时挂载，确保关闭时一定执行焦点与 inert 状态清理。
#[component]
fn ModalLayer(
    title: String,
    on_close: EventHandler<MouseEvent>,
    disable_backdrop: bool,
    disable_close: bool,
    hide_close: bool,
    children: Element,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    let modal_number = use_hook(|| NEXT_MODAL_ID.fetch_add(1, Ordering::Relaxed));
    let modal_id = format!("ains-modal-{modal_number}");
    let backdrop_id = format!("{modal_id}-backdrop");
    let title_id = format!("{modal_id}-title");
    // Capture the provider while this scope is still mounted. `use_drop`
    // runs while the component scope is being destroyed, so a regular
    // `spawn` from there would attach the cleanup task to a dead scope and
    // lose access to the document context before its first poll.
    let cleanup_document = use_hook(document::document);

    use_effect({
        let modal_id = modal_id.clone();
        move || {
            let script = modal_script(MODAL_MOUNT_SCRIPT, &modal_id);
            spawn(async move {
                let _ = document::eval(&script).await;
            });
        }
    });
    use_drop({
        let modal_id = modal_id.clone();
        let cleanup_document = cleanup_document.clone();
        move || {
            let script = modal_script(MODAL_CLEANUP_SCRIPT, &modal_id);
            // Document::eval starts evaluation when it is created. Await the
            // result from the root scope so unmounting ModalLayer cannot
            // cancel or orphan the cleanup.
            let eval = cleanup_document.eval(script);
            spawn_forever(async move {
                let _ = eval.await;
            });
        }
    });

    rsx! {
        div {
            id: backdrop_id,
            class: "ains-modal__backdrop",
            onclick: move |e| {
                if !disable_backdrop && !disable_close {
                    on_close.call(e);
                }
            },
        }
        div {
            id: modal_id,
            class: "ains-modal__wrap",
            role: "dialog",
            aria_modal: "true",
            aria_labelledby: title_id.as_str(),
            tabindex: "-1",
            "data-ains-modal-root": "true",
            "data-ains-modal-close-disabled": if disable_close { "true" } else { "false" },
            div { class: "ains-modal__card",
                header { class: "ains-modal__header",
                    h3 { id: title_id, class: "ains-modal__title", "{title}" }
                    if !hide_close {
                        button {
                            class: "ains-modal__close",
                            r#type: "button",
                            disabled: disable_close,
                            aria_label: t.modal_close,
                            "data-ains-modal-close": "true",
                            onclick: move |e| on_close.call(e),
                            X {}
                        }
                    } else {
                        button {
                            hidden: true,
                            r#type: "button",
                            disabled: disable_close,
                            "data-ains-modal-close": "true",
                            onclick: move |e| on_close.call(e),
                        }
                    }
                }
                div { class: "ains-modal__body", {children} }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use dioxus::dioxus_core::{AttributeValue, Mutation, NoOpMutations, ScopeId};
    use dioxus::document::{Document, Eval, LinkProps, NoOpDocument};

    use super::*;

    #[derive(Default)]
    struct CapturingDocument {
        scripts: RefCell<Vec<String>>,
    }

    impl Document for CapturingDocument {
        fn eval(&self, script: String) -> Eval {
            self.scripts.borrow_mut().push(script.clone());
            NoOpDocument.eval(script)
        }

        fn create_link(&self, _: LinkProps) {}
    }

    #[derive(Clone, Props)]
    struct ModalLifecycleTestProps {
        open: Rc<Cell<bool>>,
    }

    impl PartialEq for ModalLifecycleTestProps {
        fn eq(&self, other: &Self) -> bool {
            Rc::ptr_eq(&self.open, &other.open)
        }
    }

    fn modal_lifecycle_test_app(props: ModalLifecycleTestProps) -> Element {
        rsx! {
            Modal {
                title: "Lifecycle test".to_string(),
                open: props.open.get(),
                on_close: |_| {},
                "Modal content"
            }
        }
    }

    fn close_disabled_modal_test_app() -> Element {
        rsx! {
            Modal {
                title: "Disabled close test".to_string(),
                open: true,
                disable_close: true,
                on_close: |_| {},
                "Modal content"
            }
        }
    }

    #[test]
    fn disable_close_renders_the_keyboard_and_button_contract() {
        let mut dom = VirtualDom::new(close_disabled_modal_test_app);
        let mutations = dom.rebuild_to_vec();

        assert!(mutations.edits.iter().any(|mutation| matches!(
            mutation,
            Mutation::SetAttribute {
                name: "data-ains-modal-close-disabled",
                value: AttributeValue::Text(value),
                ..
            } if value == "true"
        )));
        assert!(mutations.edits.iter().any(|mutation| matches!(
            mutation,
            Mutation::SetAttribute {
                name: "disabled",
                value: AttributeValue::Bool(true),
                ..
            }
        )));
    }

    #[test]
    fn accessibility_script_manages_keyboard_focus_and_background() {
        let script = modal_script(MODAL_MOUNT_SCRIPT, "ains-modal-42");

        assert!(script.contains("ains-modal-42"));
        assert!(script.contains("event.key === \"Escape\""));
        assert!(script.contains("dialog.dataset.ainsModalCloseDisabled"));
        assert!(script.contains("event.key !== \"Tab\""));
        assert!(script.contains("sibling.inert = true"));
        assert!(script.contains("finalFocus.focus()"));
        assert!(script.contains("document.body.style.overflow = \"hidden\""));
        assert!(!script.contains("__AINS_MODAL_ID__"));
    }

    #[test]
    fn unmount_starts_cleanup_with_the_captured_document_provider() {
        let open = Rc::new(Cell::new(true));
        let document = Rc::new(CapturingDocument::default());
        let document_context: Rc<dyn Document> = document.clone();
        let mut dom = VirtualDom::new_with_props(
            modal_lifecycle_test_app,
            ModalLifecycleTestProps { open: open.clone() },
        );
        dom.in_scope(ScopeId::ROOT, || provide_context(document_context));
        dom.rebuild_in_place();

        assert!(
            !document
                .scripts
                .borrow()
                .iter()
                .any(|script| script.contains("__ainsModalState?.cleanups?.[modalId]?.()")),
            "cleanup must not run while the modal is mounted"
        );

        open.set(false);
        dom.mark_dirty(ScopeId::APP);
        dom.render_immediate(&mut NoOpMutations);

        let scripts = document.scripts.borrow();
        let cleanup = scripts
            .iter()
            .find(|script| script.contains("__ainsModalState?.cleanups?.[modalId]?.()"))
            .expect("unmount must evaluate the modal cleanup script");
        assert!(cleanup.contains("ains-modal-"));
    }

    #[test]
    fn accessibility_script_reference_counts_stacked_modal_state() {
        let script = modal_script(MODAL_MOUNT_SCRIPT, "ains-modal-9");

        assert!(script.contains("state.cleanups ??= {};"));
        assert!(script.contains("state.inertEntries ??= new Map();"));
        assert!(script.contains("state.previousOverflow ??= \"\";"));
        assert!(script.contains("state.focusOrigin ??= null;"));
        assert!(script.contains("entry.count += 1"));
        assert!(script.contains("entry.count -= 1"));
        assert!(script.contains("state.stack.push(modalId)"));
        assert!(script.contains("state.stack.lastIndexOf(modalId)"));
        assert!(script.contains("state.stack.splice(stackIndex, 1)"));
        assert!(script.contains("state.stack.forEach((id, index)"));
        assert!(script.contains("70 + index * 20"));
        assert!(script.contains("stackedDialog.inert = index !== topIndex"));
        assert!(script.contains("syncStack();"));
        assert!(!script.contains("state.openCount"));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use js_sys::{Promise, eval};
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::{Element, HtmlElement};

    use super::{MODAL_CLEANUP_SCRIPT, MODAL_MOUNT_SCRIPT, modal_script};

    wasm_bindgen_test_configure!(run_in_browser);

    async fn flush_microtasks() {
        JsFuture::from(Promise::resolve(&JsValue::UNDEFINED))
            .await
            .expect("microtask checkpoint");
    }

    fn eval_bool(expression: &str) -> bool {
        eval(expression)
            .unwrap_or_else(|error| panic!("JavaScript failed for {expression:?}: {error:?}"))
            .as_bool()
            .unwrap_or_else(|| panic!("JavaScript did not return bool for {expression:?}"))
    }

    fn eval_string(expression: &str) -> String {
        eval(expression)
            .unwrap_or_else(|error| panic!("JavaScript failed for {expression:?}: {error:?}"))
            .as_string()
            .unwrap_or_else(|| panic!("JavaScript did not return string for {expression:?}"))
    }

    fn modal_markup(id: &str) -> String {
        format!(
            r#"<div id="{id}-backdrop" class="ains-modal__backdrop"></div>
               <div id="{id}" data-ains-modal-root="true" tabindex="-1">
                 <input id="{id}-first">
                 <button id="{id}-last" data-ains-modal-close="true">close</button>
               </div>"#
        )
    }

    struct BrowserFixture {
        body: HtmlElement,
        root: Element,
        original_overflow: String,
    }

    impl Drop for BrowserFixture {
        fn drop(&mut self) {
            // Keep later browser tests isolated even when this test unwinds
            // after a failed assertion.
            let _ = eval(
                "for (const cleanup of Object.values(window.__ainsModalState?.cleanups ?? {})) cleanup(); \
                 delete window.__ainsModalState; delete window.__modalEscape;",
            );
            let _ = self
                .body
                .style()
                .set_property("overflow", &self.original_overflow);
            self.root.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn stacked_modals_follow_open_order_and_restore_global_dom_state() {
        let window = web_sys::window().expect("browser window");
        let document = window.document().expect("browser document");
        let body = document.body().expect("document body");
        let test_root = document.create_element("div").expect("test root");
        let fixture = BrowserFixture {
            body: body.clone(),
            root: test_root.clone(),
            original_overflow: body
                .style()
                .get_property_value("overflow")
                .expect("read original body overflow"),
        };
        test_root.set_id("modal-browser-test-root");
        test_root.set_inner_html(&format!(
            r#"<button id="focus-origin">open</button>
               <main id="background">background</main>
               {}"#,
            modal_markup("modal-b")
        ));
        body.append_child(&test_root).expect("attach test root");
        body.style().set_property("overflow", "auto").unwrap();
        // Simulate state retained from an older bundle that only knew about
        // the modal stack. Mounting must backfill every newer state member
        // before it registers cleanup or inert bookkeeping.
        eval("window.__ainsModalState = { stack: [] }; delete window.__modalEscape;")
            .expect("install partial legacy modal state");

        document
            .get_element_by_id("focus-origin")
            .expect("focus origin")
            .dyn_into::<HtmlElement>()
            .expect("focus origin element")
            .focus()
            .expect("focus origin");

        eval(&modal_script(MODAL_MOUNT_SCRIPT, "modal-b")).expect("mount modal B");
        flush_microtasks().await;
        assert!(eval_bool(
            "typeof window.__ainsModalState.cleanups === 'object'"
        ));
        assert!(eval_bool(
            "window.__ainsModalState.inertEntries instanceof Map"
        ));
        assert_eq!(eval_string("document.activeElement.id"), "modal-b-first");
        assert_eq!(eval_string("document.body.style.overflow"), "hidden");
        assert!(eval_bool("document.getElementById('background').inert"));
        assert_eq!(
            eval_string("document.getElementById('modal-b').style.zIndex"),
            "70"
        );

        // Insert A before B in DOM order, but mount it afterwards. Opening
        // order, not document order, must decide keyboard and visual priority.
        test_root
            .insert_adjacent_html("afterbegin", &modal_markup("modal-a"))
            .expect("insert modal A before modal B");
        eval("document.querySelector('#modal-a [data-ains-modal-close]').addEventListener('click', () => { window.__modalEscape = 'a'; }); document.querySelector('#modal-b [data-ains-modal-close]').addEventListener('click', () => { window.__modalEscape = 'b'; });")
            .expect("install close observers");
        eval(&modal_script(MODAL_MOUNT_SCRIPT, "modal-a")).expect("mount modal A");
        flush_microtasks().await;

        assert_eq!(eval_string("document.activeElement.id"), "modal-a-first");
        assert!(eval_bool("document.getElementById('modal-b').inert"));
        assert_eq!(
            eval_string("document.getElementById('modal-a').style.zIndex"),
            "90"
        );
        assert_eq!(
            eval_string("document.getElementById('modal-a-backdrop').style.zIndex"),
            "80"
        );

        eval("document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true }));")
            .expect("dispatch reverse tab");
        assert_eq!(eval_string("document.activeElement.id"), "modal-a-last");
        eval("document.getElementById('modal-a').dataset.ainsModalCloseDisabled = 'true'; delete window.__modalEscape; document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));")
            .expect("dispatch disabled escape");
        assert!(eval_bool("window.__modalEscape === undefined"));
        eval("document.getElementById('modal-a').dataset.ainsModalCloseDisabled = 'false'; document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));")
            .expect("dispatch enabled escape");
        assert_eq!(eval_string("window.__modalEscape"), "a");

        eval(&modal_script(MODAL_CLEANUP_SCRIPT, "modal-a")).expect("cleanup modal A");
        flush_microtasks().await;
        assert!(!eval_bool("document.getElementById('modal-b').inert"));
        assert_eq!(eval_string("document.activeElement.id"), "modal-b-first");
        assert_eq!(eval_string("document.body.style.overflow"), "hidden");
        assert!(eval_bool("document.getElementById('background').inert"));

        // Reopen A, then close the lower B before opening C. Remaining layers
        // must be compacted so C receives a unique layer above A instead of
        // reusing A's old z-index.
        eval(&modal_script(MODAL_MOUNT_SCRIPT, "modal-a")).expect("reopen modal A");
        flush_microtasks().await;
        assert_eq!(
            eval_string("document.getElementById('modal-a').style.zIndex"),
            "90"
        );
        eval(&modal_script(MODAL_CLEANUP_SCRIPT, "modal-b")).expect("cleanup non-top modal B");
        flush_microtasks().await;
        assert_eq!(
            eval_string("document.getElementById('modal-a').style.zIndex"),
            "70"
        );
        assert_eq!(
            eval_string("document.getElementById('modal-a-backdrop').style.zIndex"),
            "60"
        );

        test_root
            .insert_adjacent_html("afterbegin", &modal_markup("modal-c"))
            .expect("insert modal C before remaining modal A");
        eval(&modal_script(MODAL_MOUNT_SCRIPT, "modal-c")).expect("mount modal C");
        flush_microtasks().await;
        assert_eq!(
            eval_string("document.getElementById('modal-a').style.zIndex"),
            "70"
        );
        assert_eq!(
            eval_string("document.getElementById('modal-c').style.zIndex"),
            "90"
        );
        assert_eq!(
            eval_string("document.getElementById('modal-c-backdrop').style.zIndex"),
            "80"
        );

        eval(&modal_script(MODAL_CLEANUP_SCRIPT, "modal-c")).expect("cleanup modal C");
        eval(&modal_script(MODAL_CLEANUP_SCRIPT, "modal-a")).expect("cleanup modal A");
        flush_microtasks().await;
        assert!(!eval_bool("document.getElementById('background').inert"));
        assert_eq!(eval_string("document.body.style.overflow"), "auto");
        assert_eq!(eval_string("document.activeElement.id"), "focus-origin");

        // Both dialog nodes exist before either mount effect runs, matching a
        // render that opens two independent Modal components at once. The
        // first effect may inert E as a sibling, but E's own effect must make
        // it interactive when it becomes the top stack entry.
        test_root
            .insert_adjacent_html(
                "beforeend",
                &format!("{}{}", modal_markup("modal-d"), modal_markup("modal-e")),
            )
            .expect("insert simultaneously rendered modals D and E");
        eval(&modal_script(MODAL_MOUNT_SCRIPT, "modal-d")).expect("mount modal D");
        eval(&modal_script(MODAL_MOUNT_SCRIPT, "modal-e")).expect("mount modal E");
        flush_microtasks().await;
        assert!(eval_bool("document.getElementById('modal-d').inert"));
        assert!(!eval_bool("document.getElementById('modal-e').inert"));
        assert_eq!(eval_string("document.activeElement.id"), "modal-e-first");

        eval(&modal_script(MODAL_CLEANUP_SCRIPT, "modal-e")).expect("cleanup modal E");
        flush_microtasks().await;
        assert!(!eval_bool("document.getElementById('modal-d').inert"));
        assert_eq!(eval_string("document.activeElement.id"), "modal-d-first");

        eval(&modal_script(MODAL_CLEANUP_SCRIPT, "modal-d")).expect("cleanup modal D");
        flush_microtasks().await;
        assert!(!eval_bool("document.getElementById('background').inert"));
        assert_eq!(eval_string("document.body.style.overflow"), "auto");
        assert_eq!(eval_string("document.activeElement.id"), "focus-origin");

        drop(fixture);
    }
}
