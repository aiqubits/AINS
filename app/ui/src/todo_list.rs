use dioxus::prelude::*;
use dioxus_icons::lucide::{Circle, CircleCheck};

use crate::{EN, I18nContext};

/// 单条待办（视图模型；宿主从 todo_write 工具输出解析）。
#[derive(Debug, Clone, PartialEq)]
pub struct TodoItemView {
    pub text: String,
    pub done: bool,
}

/// Todo 列表展示（Phase 6.12）：呈现智能体当前待办清单（只读）。
#[component]
pub fn TodoList(todos: ReadSignal<Vec<TodoItemView>>) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    let items = todos.read().clone();
    let remaining = items.iter().filter(|i| !i.done).count();

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/todo_list.css") }
        section { class: "ains-todo",
            header { class: "ains-todo__header",
                span { class: "ains-todo__title", {t.todo_title} }
                if !items.is_empty() {
                    span { class: "ains-todo__count", "{remaining}/{items.len()}" }
                }
            }
            if items.is_empty() {
                p { class: "ains-todo__empty", {t.todo_empty} }
            }
            ul { class: "ains-todo__list",
                for (idx , item) in items.iter().enumerate() {
                    li {
                        key: "{idx}",
                        class: if item.done { "ains-todo__item ains-todo__item--done" } else { "ains-todo__item" },
                        if item.done {
                            CircleCheck { class: "ains-todo__icon ains-todo__icon--done" }
                        } else {
                            Circle { class: "ains-todo__icon" }
                        }
                        span { class: "ains-todo__text", "{item.text}" }
                    }
                }
            }
        }
    }
}

/// 从 todo_write 工具输出（Markdown 复选行）解析待办项。
///
/// 识别 `- [ ] text` / `- [x] text`（大小写不敏感），其余行忽略。
pub fn parse_todo_markdown(output: &str) -> Vec<TodoItemView> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let rest = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))?;
            let (marker, text) = rest.split_at(
                rest.char_indices()
                    .nth(3)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len()),
            );
            let marker = marker.to_ascii_lowercase();
            if marker.starts_with("[x]") {
                Some(TodoItemView {
                    text: text.trim().to_string(),
                    done: true,
                })
            } else if marker.starts_with("[ ]") {
                Some(TodoItemView {
                    text: text.trim().to_string(),
                    done: false,
                })
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checkbox_lines() {
        let md = "# Todos\n- [ ] read files\n- [x] write report\n* [ ] ship it\nnot a todo\n";
        let items = parse_todo_markdown(md);
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0],
            TodoItemView {
                text: "read files".into(),
                done: false
            }
        );
        assert_eq!(
            items[1],
            TodoItemView {
                text: "write report".into(),
                done: true
            }
        );
        assert_eq!(
            items[2],
            TodoItemView {
                text: "ship it".into(),
                done: false
            }
        );
    }

    #[test]
    fn ignores_non_todo_content() {
        assert!(parse_todo_markdown("just prose\n## heading\n").is_empty());
    }
}
