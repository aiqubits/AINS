//! Memory 浏览器视图（Phase 6.6 + 向量表生产路径 P2 `/memory` 向量搜索）：
//! 展示 memdir 长期记忆库 + 生产 durable vector memory manifest（§9.4）。

use dioxus::prelude::*;

use agent_core::memory::{MemdirEntry, MemdirStore, MemoryHit, format_iso_utc, parse_iso_utc};
use ui::{I18nContext, MemoryCard, MemoryViewer};

use crate::agent::service;

/// memdir 单条 scan 上限（避免一次拉入过多）。
const MAX_MEMORIES: usize = 500;

fn to_card(entry: &MemdirEntry) -> MemoryCard {
    // created_at 已是 ISO；转成 YYYY-MM-DD 展示（解析失败则原样）。
    let created_at = parse_iso_utc(&entry.created_at)
        .map(|ms| format_iso_utc(ms).chars().take(10).collect::<String>())
        .unwrap_or_else(|| entry.created_at.clone());
    MemoryCard {
        id: entry.id.clone(),
        name: entry.name.clone(),
        description: entry.description.clone(),
        category: entry.category.clone(),
        importance: entry.importance,
        tags: entry.tags.clone(),
        created_at,
        body: entry.body.clone(),
    }
}

#[component]
pub fn Memory() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    let client = use_context::<client_api::Client>();
    let memories = use_signal(Vec::<MemoryCard>::new);
    // 生产 durable vector memory manifest 行（§9.4 格式）。
    let mut durable = use_signal(Vec::<String>::new);
    let mut search_query = use_signal(String::new);
    let mut search_results = use_signal(Vec::<MemoryHit>::new);
    let mut search_error = use_signal(|| None::<String>);
    // Async searches may complete out of order. Keep a monotonically
    // increasing generation so a slow, older query cannot replace the results
    // for the query the user most recently submitted or edited.
    let mut search_generation = use_signal(|| 0u64);
    let mut error = use_signal(|| None::<String>);
    let manifest_client = client.clone();

    use_future(move || {
        let client = manifest_client.clone();
        async move {
            match service::open_memory_store(client.clone()).await {
                Ok(store) => load_into(&store, memories, error).await,
                Err(err) => error.set(Some(err)),
            }
            // durable manifest 独立加载：失败只提示，不阻断 memdir 展示。
            match service::open_durable_manifest(client).await {
                Ok(lines) => durable.set(lines),
                Err(err) => error.set(Some(format!("durable memory: {err}"))),
            }
        }
    });

    let durable_items: Vec<String> = durable.read().clone();
    let durable_count = durable_items.len();
    let mut run_search = move || {
        let query = search_query().trim().to_string();
        let generation = search_generation().saturating_add(1);
        search_generation.set(generation);
        if query.is_empty() {
            search_results.set(Vec::new());
            search_error.set(None);
            return;
        }
        let client = client.clone();
        spawn(async move {
            match service::search_durable_memory(client, &query, 10).await {
                Ok(hits) => {
                    if search_generation() == generation {
                        search_results.set(hits);
                        search_error.set(None);
                    }
                }
                Err(err) => {
                    if search_generation() == generation {
                        search_error.set(Some(err));
                    }
                }
            }
        });
    };
    let results: Vec<MemoryHit> = search_results.read().clone();
    rsx! {
        div { style: "padding:16px;",
            if let Some(err) = error.read().as_ref() {
                div { style: "margin-bottom:12px;color:var(--color-error-text);font-size:13px;",
                    "{t.memory_load_failed}: {err}"
                }
            }
            if !durable_items.is_empty() {
                div { style: "margin-bottom:16px;",
                    div { style: "font-size:13px;font-weight:600;margin-bottom:8px;",
                        "Durable Memory ({durable_count})"
                    }
                    div { style: "display:flex;flex-direction:column;gap:6px;font-size:13px;",
                        {durable_items.iter().map(|line| rsx! {
                            div { style: "padding:8px 12px;border-radius:var(--radius-xl);border:1px solid var(--color-border-default);background:var(--color-surface);",
                                "{line}"
                            }
                        })}
                    }
                }
            }
            div { style: "margin-bottom:16px;",
                div { style: "font-size:13px;font-weight:600;margin-bottom:8px;",
                    "Search Durable Memory"
                }
                form {
                    style: "display:flex;gap:8px;",
                    onsubmit: move |event| {
                        event.prevent_default();
                        run_search();
                    },
                    input {
                        r#type: "search",
                        value: "{search_query}",
                        placeholder: "Search remembered context",
                        style: "flex:1;padding:8px;border:1px solid var(--color-border-default);border-radius:var(--radius-xl);",
                        oninput: move |event| {
                            search_query.set(event.value());
                            search_generation.set(search_generation().saturating_add(1));
                            search_results.set(Vec::new());
                            search_error.set(None);
                        },
                    }
                    button { r#type: "submit", "Search" }
                }
                if let Some(err) = search_error.read().as_ref() {
                    div { style: "margin-top:8px;color:var(--color-error-text);font-size:13px;", "{err}" }
                }
                if !results.is_empty() {
                    div { style: "display:flex;flex-direction:column;gap:6px;margin-top:8px;font-size:13px;",
                        for hit in results {
                            div { style: "padding:8px 12px;border-radius:var(--radius-xl);border:1px solid var(--color-border-default);background:var(--color-surface);",
                                strong { "{hit.title}" }
                                div { "{hit.content}" }
                            }
                        }
                    }
                }
            }
            MemoryViewer { memories }
        }
    }
}

async fn load_into(
    store: &MemdirStore,
    mut memories: Signal<Vec<MemoryCard>>,
    mut error: Signal<Option<String>>,
) {
    match store.scan(MAX_MEMORIES).await {
        Ok(entries) => memories.set(entries.iter().map(to_card).collect()),
        Err(err) => error.set(Some(err.to_string())),
    }
}
