use dioxus::prelude::*;

use crate::{EN, I18nContext};

/// 通用数据表 —— 按 DESIGN.md §3.9 规格实现。
///
/// 风格：毛玻璃容器、24px 圆角、表头 `bg-slate-50/70` + 大写 caption 字体、
/// 行 hover `bg-slate-50/40`。
///
/// 本组件**不**关心数据渲染逻辑 —— 调用方将每行预渲染为 `<tr>` 后传入，
/// 这样可以避开 Dioxus 中 `fn(&T) -> Element` 闭包作为 prop 的限制，
/// 同时让 `T` 可以是任何类型（包括 `client_api::UserResponse`）。
///
/// Body-cell alignment is supported for at most [`DATA_TABLE_MAX_COLUMNS`]
/// columns. This is a component-authoring limit rather than user-controlled
/// input; exceeding it fails during rendering instead of silently falling back
/// to incorrect alignment.
pub const DATA_TABLE_MAX_COLUMNS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    fn class(&self) -> &'static str {
        match self {
            Self::Left => "ains-table__align--left",
            Self::Center => "ains-table__align--center",
            Self::Right => "ains-table__align--right",
        }
    }

    fn css_value(&self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub header: String,
    /// Tailwind 风格的宽度类，如 `"w-24"`、`"w-40"`、`"w-44"`。
    pub width: Option<String>,
    pub align: Align,
}

impl Column {
    pub fn new(header: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            width: None,
            align: Align::Left,
        }
    }

    pub fn width(mut self, w: impl Into<String>) -> Self {
        self.width = Some(w.into());
        self
    }

    pub fn align(mut self, a: Align) -> Self {
        self.align = a;
        self
    }
}

/// Encode each column's alignment as a table-scoped CSS custom property.
///
/// The shared stylesheet consumes these properties for body cells via
/// `:nth-child`, so a column's header and every data row always use the same
/// alignment without requiring each caller to repeat classes on every `<td>`.
fn column_alignment_style(columns: &[Column]) -> String {
    assert!(
        columns.len() <= DATA_TABLE_MAX_COLUMNS,
        "DataTable supports at most {DATA_TABLE_MAX_COLUMNS} columns, got {}",
        columns.len()
    );

    let mut style = String::new();
    for (index, column) in columns.iter().enumerate() {
        style.push_str("--ains-table-col-");
        style.push_str(&(index + 1).to_string());
        style.push_str("-align:");
        style.push_str(column.align.css_value());
        style.push(';');
    }
    style
}

#[component]
pub fn DataTable(
    columns: Vec<Column>,
    rows: Vec<Element>,
    #[props(default)] empty: Option<Element>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);
    let alignment_style = column_alignment_style(&columns);

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/styling/data_table.css") }
        div { class: "ains-table",
            table { class: "ains-table__table", style: alignment_style,
                thead { class: "ains-table__head",
                    tr {
                        for col in columns.iter() {
                            th {
                                class: "ains-table__th {col.align.class()} {col.width.clone().unwrap_or_default()}",
                                scope: "col",
                                "{col.header}"
                            }
                        }
                    }
                }
                tbody { class: "ains-table__body",
                    if rows.is_empty() {
                        if let Some(placeholder) = empty {
                            tr {
                                td {
                                    class: "ains-table__empty",
                                    colspan: columns.len(),
                                    {placeholder}
                                }
                            }
                        } else {
                            tr {
                                td {
                                    class: "ains-table__empty",
                                    colspan: columns.len(),
                                    {t.data_table_no_data}
                                }
                            }
                        }
                    } else {
                        for row in rows.iter() {
                            {row}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use dioxus::dioxus_core::{AttributeValue, Mutation, VirtualDom};

    use super::*;

    const DATA_TABLE_CSS: &str = include_str!("../assets/styling/data_table.css");

    fn aligned_data_table_test_app() -> Element {
        let columns = vec![
            Column::new("name").align(Align::Left),
            Column::new("status").align(Align::Center),
            Column::new("amount").align(Align::Right),
        ];
        let rows = vec![rsx! {
            tr {
                td { "Example" }
                td { "Active" }
                td { "42" }
            }
        }];

        rsx! { DataTable { columns, rows } }
    }

    #[test]
    fn rendered_table_exposes_body_alignment_properties_in_column_order() {
        let mut dom = VirtualDom::new(aligned_data_table_test_app);
        let mutations = dom.rebuild_to_vec();

        assert!(mutations.edits.iter().any(|mutation| matches!(
            mutation,
            Mutation::SetAttribute {
                name: "style",
                value: AttributeValue::Text(value),
                ..
            } if value == "--ains-table-col-1-align:left;--ains-table-col-2-align:center;--ains-table-col-3-align:right;"
        )), "rendered DataTable is missing its column-alignment contract: {:?}", mutations.edits);
    }

    #[test]
    #[should_panic(expected = "DataTable supports at most 12 columns")]
    fn exceeding_the_supported_column_count_fails_instead_of_silently_misaligning() {
        let columns = (0..=DATA_TABLE_MAX_COLUMNS)
            .map(|index| Column::new(format!("column-{index}")))
            .collect::<Vec<_>>();

        let _ = column_alignment_style(&columns);
    }

    #[test]
    fn stylesheet_supports_the_declared_column_limit_and_used_widths() {
        for index in 1..=DATA_TABLE_MAX_COLUMNS {
            assert!(
                DATA_TABLE_CSS.contains(&format!("--ains-table-col-{index}-align")),
                "missing body alignment rule for column {index}"
            );
        }

        for width in [16, 20, 24, 28, 32, 36, 40, 44, 48] {
            assert!(
                DATA_TABLE_CSS.contains(&format!(".w-{width} {{")),
                "missing table width utility w-{width}"
            );
        }
    }
}
