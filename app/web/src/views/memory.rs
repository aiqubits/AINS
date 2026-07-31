//! Memory 浏览器视图（Phase 6.6）：展示 memdir 长期记忆库。

use dioxus::prelude::*;

use agent_core::memory::{MemdirEntry, MemdirStore, format_iso_utc, parse_iso_utc};
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
    let memories = use_signal(Vec::<MemoryCard>::new);
    let mut error = use_signal(|| None::<String>);

    use_future(move || async move {
        match service::open_memory_store().await {
            Ok(store) => load_into(&store, memories, error).await,
            Err(err) => error.set(Some(err)),
        }
    });

    rsx! {
        div { style: "padding:16px;",
            if let Some(err) = error.read().as_ref() {
                div { style: "margin-bottom:12px;color:var(--color-error-text);font-size:13px;",
                    "{t.memory_load_failed}: {err}"
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
