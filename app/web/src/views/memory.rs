//! Memory 浏览器视图（Phase 6.6 + 向量表生产路径 P2 `/memory` 向量搜索）：
//! 展示 memdir 长期记忆库 + 生产 durable vector memory manifest（§9.4）。

use std::sync::Arc;

use dioxus::prelude::*;

use rust_agent::memory::{
    DurableMemoryManifestItem, MemdirEntry, MemdirStore, MemoryHit, format_iso_utc, parse_iso_utc,
};
use ui::{I18nContext, MemoryCard, MemoryViewer, Modal, Translations, tf};

use crate::agent::service;

/// memdir 单条 scan 上限（避免一次拉入过多）。
const MAX_MEMORIES: usize = 500;

fn next_generation(generation: u64) -> u64 {
    generation.saturating_add(1)
}

#[derive(Debug, PartialEq, Eq)]
enum ClearMemoryLibraryError {
    Durable(String),
    Memdir(String),
}

/// 清空的两部分必须保持顺序：durable memory 是实际参与召回的权威数据，
/// 因此它失败时不能继续清理仅用于浏览的 memdir。反之，即使 memdir 尚未
/// 成功打开，也不能阻止用户删除已展示的 durable memories。
async fn clear_memory_library<D, M>(
    clear_durable: D,
    clear_memdir: M,
) -> Result<(), ClearMemoryLibraryError>
where
    D: std::future::Future<Output = Result<(), String>>,
    M: std::future::Future<Output = Result<(), String>>,
{
    clear_durable
        .await
        .map_err(ClearMemoryLibraryError::Durable)?;
    clear_memdir.await.map_err(ClearMemoryLibraryError::Memdir)
}

async fn clear_durable_entries(client: client_api::Client) -> Result<(), String> {
    service::clear_durable_memories(client).await.map(|_| ())
}

async fn clear_memdir_entries(store: Option<Arc<MemdirStore>>) -> Result<(), String> {
    let Some(store) = store else {
        return Err("memdir storage is unavailable".to_string());
    };
    store
        .clear_entries()
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn refresh_durable_manifest(
    t: &'static Translations,
    client: client_api::Client,
    mut durable: Signal<Vec<DurableMemoryManifestItem>>,
    mut error: Signal<Option<String>>,
    mut generation: Signal<u64>,
) {
    let expected = next_generation(generation());
    generation.set(expected);
    spawn(async move {
        match service::open_durable_manifest(client).await {
            Ok(items) if generation() == expected => durable.set(items),
            Ok(_) => {}
            Err(err) if generation() == expected => {
                error.set(Some(format!("{}: {err}", t.memory_durable_load_failed)))
            }
            Err(_) => {}
        }
    });
}

#[derive(Clone)]
struct PendingDurableDelete {
    id: String,
    title: String,
}

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
    let mut store_sig = use_signal(|| None::<Arc<MemdirStore>>);
    // 保留 durable 条目的 canonical id，页面可直接删除而无需先做语义搜索。
    let mut durable = use_signal(Vec::<DurableMemoryManifestItem>::new);
    let mut search_query = use_signal(String::new);
    let mut search_results = use_signal(Vec::<MemoryHit>::new);
    let mut pending_durable_delete = use_signal(|| None::<PendingDurableDelete>);
    let mut search_error = use_signal(|| None::<String>);
    // Async searches may complete out of order. Keep a monotonically
    // increasing generation so a slow, older query cannot replace the results
    // for the query the user most recently submitted or edited.
    let mut search_generation = use_signal(|| 0u64);
    // Mutation-triggered manifest loads can complete out of order just like
    // searches. Only the latest completed mutation may update the UI.
    let durable_refresh_generation = use_signal(|| 0u64);
    let mut error = use_signal(|| None::<String>);
    let mut auto_extract = use_signal(|| true);
    // Initial preference loading and rapid checkbox changes can finish out of
    // order. Only the newest request is allowed to change the rendered value.
    let mut auto_extract_generation = use_signal(|| 0u64);
    let manifest_client = client.clone();
    let search_client = client.clone();
    let clear_client = client.clone();
    let durable_delete_client = client.clone();
    let auto_extract_client = client.clone();

    use_future(move || {
        let client = manifest_client.clone();
        async move {
            // Reserve the generation before the first await. Otherwise a
            // click while the initial library requests are in flight can get
            // an older generation and later be overwritten by this stale
            // preference read.
            let auto_extract_load_generation = next_generation(auto_extract_generation());
            auto_extract_generation.set(auto_extract_load_generation);
            match service::open_memory_store(client.clone()).await {
                Ok(store) => {
                    load_into(t, &store, memories, error).await;
                    store_sig.set(Some(store));
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.memory_load_failed))),
            }
            // durable manifest 独立加载：失败只提示，不阻断 memdir 展示。
            // 与 `refresh_durable_manifest` 一致，失败分支也用代际守卫，避免
            // 初始加载的过期错误覆盖较新的 UI 状态。
            let generation = durable_refresh_generation();
            match service::open_durable_manifest(client.clone()).await {
                Ok(lines) if durable_refresh_generation() == generation => durable.set(lines),
                Ok(_) => {}
                Err(err) if durable_refresh_generation() == generation => {
                    error.set(Some(format!("{}: {err}", t.memory_durable_load_failed)))
                }
                Err(_) => {}
            }
            match service::memory_auto_extract_enabled(client).await {
                Ok(enabled) if auto_extract_generation() == auto_extract_load_generation => {
                    auto_extract.set(enabled)
                }
                Ok(_) => {}
                Err(err) if auto_extract_generation() == auto_extract_load_generation => error.set(
                    Some(format!("{}: {err}", t.memory_auto_extract_save_failed)),
                ),
                Err(_) => {}
            }
        }
    });

    let durable_items: Vec<DurableMemoryManifestItem> = durable.read().clone();
    let durable_count = durable_items.len();
    let mut run_search = move || {
        let query = search_query().trim().to_string();
        let generation = next_generation(search_generation());
        search_generation.set(generation);
        if query.is_empty() {
            search_results.set(Vec::new());
            search_error.set(None);
            return;
        }
        let client = search_client.clone();
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
    let reload = move || async move {
        let store = store_sig.read().clone();
        if let Some(store) = store {
            load_into(t, &store, memories, error).await;
        }
    };
    let on_delete = move |id: String| {
        let Some(store) = store_sig.read().clone() else {
            return;
        };
        spawn(async move {
            match store.delete_entry_by_id(&id).await {
                Ok(true) => reload().await,
                Ok(false) => {
                    // 另一页面/任务可能已先删除该条目；同步权威存储以移除
                    // 本页的过期卡片，而不是让用户面对无法再次删除的条目。
                    reload().await;
                    error.set(Some(t.memory_delete_failed.to_string()));
                }
                Err(err) => {
                    // 删除条目与更新索引不是一个事务；后一步失败时前一步可能
                    // 已完成，必须从权威存储重载，不能把陈旧卡片留在页面上。
                    reload().await;
                    error.set(Some(format!("{}: {err}", t.memory_delete_failed)));
                }
            }
        });
    };
    let on_clear_all = move |_| {
        let store = store_sig.read().clone();
        let client = clear_client.clone();
        spawn(async move {
            match clear_memory_library(
                clear_durable_entries(client.clone()),
                clear_memdir_entries(store),
            )
            .await
            {
                Ok(()) => {
                    reload().await;
                    search_generation.set(next_generation(search_generation()));
                    search_results.set(Vec::new());
                    refresh_durable_manifest(t, client, durable, error, durable_refresh_generation);
                }
                Err(ClearMemoryLibraryError::Durable(err)) => {
                    // durable 批量删除允许部分完成；即使返回错误也必须重载，
                    // 不能继续把已删除条目留在页面上。此时不触碰 memdir，
                    // 避免 UI 报错却把仅展示层清空而长期上下文仍保留。
                    // 但部分删除后语义搜索结果可能包含已删除的命中，必须
                    // 失效本地搜索视图（与 Memdir 失败分支一致）。
                    search_generation.set(next_generation(search_generation()));
                    search_results.set(Vec::new());
                    refresh_durable_manifest(t, client, durable, error, durable_refresh_generation);
                    error.set(Some(format!("{}: {err}", t.memory_clear_all_failed)));
                }
                Err(ClearMemoryLibraryError::Memdir(err)) => {
                    // durable 已成功，即使 memdir 未初始化或仅部分清理，也必须
                    // 失效语义搜索结果并刷新 durable 清单；否则页面会继续展示
                    // 已删除的命中。随后保留部分完成错误供用户重试 memdir。
                    reload().await;
                    search_generation.set(next_generation(search_generation()));
                    search_results.set(Vec::new());
                    refresh_durable_manifest(t, client, durable, error, durable_refresh_generation);
                    error.set(Some(format!("{}: {err}", t.memory_clear_all_partial)));
                }
            }
        });
    };
    let on_delete_durable = move |id: String| {
        let client = durable_delete_client.clone();
        spawn(async move {
            match service::delete_durable_memory(client.clone(), &id).await {
                Ok(true) => {
                    search_results.write().retain(|hit| hit.id != id);
                    refresh_durable_manifest(t, client, durable, error, durable_refresh_generation);
                }
                Ok(false) => {
                    refresh_durable_manifest(t, client, durable, error, durable_refresh_generation);
                    error.set(Some(t.memory_delete_failed.to_string()));
                }
                Err(err) => {
                    // `forget` 可能在派生索引清理后失败；重新读取权威
                    // memories 表，以免 UI 保留错误的本地快照。
                    refresh_durable_manifest(t, client, durable, error, durable_refresh_generation);
                    error.set(Some(format!("{}: {err}", t.memory_delete_failed)));
                }
            }
        });
    };
    let on_auto_extract_change = move |event: Event<FormData>| {
        let enabled = event.checked();
        let previous = auto_extract();
        let generation = next_generation(auto_extract_generation());
        auto_extract_generation.set(generation);
        auto_extract.set(enabled);
        let client = auto_extract_client.clone();
        spawn(async move {
            // Earlier writes are allowed to finish, but they must then replay
            // the newest desired value. This makes rapid off→on toggles
            // converge in storage as well as in the UI.
            let mut previous = previous;
            let mut desired = enabled;
            let mut expected = generation;
            loop {
                if let Err(err) =
                    service::set_memory_auto_extract_enabled(client.clone(), desired).await
                {
                    if auto_extract_generation() == expected {
                        // 回滚到“最近一次成功提交后 UI 应显示的值”：每次成功
                        // 回放都刷新 previous，避免连点后最后一轮失败时回滚
                        // 到最早捕获的状态（review P3）。
                        auto_extract.set(previous);
                        error.set(Some(format!(
                            "{}: {err}",
                            t.memory_auto_extract_save_failed
                        )));
                    }
                    break;
                }
                if auto_extract_generation() == expected {
                    break;
                }
                previous = auto_extract();
                expected = auto_extract_generation();
                desired = auto_extract();
            }
        });
    };
    rsx! {
        div { style: "padding:16px;",
            // 各写入路径已在 error 信号里带上完整的操作前缀（如删除失败、
            // 自动提取设置保存失败），这里不再重复叠加"加载失败"前缀。
            if let Some(err) = error.read().as_ref() {
                div { style: "margin-bottom:12px;color:var(--color-error-text);font-size:13px;",
                    "{err}"
                }
            }
            if !durable_items.is_empty() {
                div { style: "margin-bottom:16px;",
                    div { style: "font-size:13px;font-weight:600;margin-bottom:8px;",
                        {tf(t.memory_durable_title, &[("count", &durable_count)])}
                    }
                    div { style: "display:flex;flex-direction:column;gap:6px;font-size:13px;",
                        {durable_items.iter().map(|item| rsx! {
                            div { style: "padding:8px 12px;border-radius:var(--radius-xl);border:1px solid var(--color-border-default);background:var(--color-surface);",
                                strong { "[{item.memory_type.as_str()}] {item.title} ({item.age})" }
                                div { "{item.description}" }
                                button {
                                    r#type: "button",
                                    onclick: {
                                        let item = item.clone();
                                        move |_| pending_durable_delete.set(Some(PendingDurableDelete {
                                            id: item.id.clone(),
                                            title: item.title.clone(),
                                        }))
                                    },
                                    {t.memory_delete_btn}
                                }
                            }
                        })}
                    }
                }
            }
            div { style: "display:flex;align-items:flex-start;gap:8px;margin-bottom:16px;padding:10px;border:1px solid var(--color-border-default);border-radius:var(--radius-xl);",
                input {
                    id: "memory-auto-extract",
                    r#type: "checkbox",
                    checked: auto_extract(),
                    onchange: on_auto_extract_change,
                }
                label { r#for: "memory-auto-extract",
                    strong { "{t.memory_auto_extract_label}" }
                    div { style: "font-size:12px;color:var(--color-text-secondary);", "{t.memory_auto_extract_hint}" }
                }
            }
            div { style: "margin-bottom:16px;",
                div { style: "font-size:13px;font-weight:600;margin-bottom:8px;",
                    {t.memory_search_title}
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
                        placeholder: t.memory_search_placeholder,
                        style: "flex:1;padding:8px;border:1px solid var(--color-border-default);border-radius:var(--radius-xl);",
                        oninput: move |event| {
                            search_query.set(event.value());
                            search_generation.set(next_generation(search_generation()));
                            search_results.set(Vec::new());
                            search_error.set(None);
                        },
                    }
                    button { r#type: "submit", {t.memory_search_btn} }
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
                                button {
                                    r#type: "button",
                                    onclick: {
                                        let hit = hit.clone();
                                        move |_| pending_durable_delete.set(Some(PendingDurableDelete {
                                            id: hit.id.clone(),
                                            title: hit.title.clone(),
                                        }))
                                    },
                                    {t.memory_delete_btn}
                                }
                            }
                        }
                    }
                }
            }
            MemoryViewer { memories, has_durable_memories: durable_count > 0, on_delete, on_clear_all }

            if let Some(hit) = pending_durable_delete.read().clone() {
                Modal {
                    title: t.memory_confirm_delete_title.to_string(),
                    on_close: move |_| pending_durable_delete.set(None),
                    div { style: "display:flex;flex-direction:column;gap:12px;",
                        p { {tf(t.memory_confirm_delete_msg, &[("name", &hit.title)])} }
                        div { style: "display:flex;justify-content:flex-end;gap:8px;",
                            button {
                                r#type: "button",
                                onclick: move |_| pending_durable_delete.set(None),
                                {t.modal_close}
                            }
                            button {
                                r#type: "button",
                                onclick: {
                                    let id = hit.id.clone();
                                    move |_| {
                                        pending_durable_delete.set(None);
                                        on_delete_durable(id.clone());
                                    }
                                },
                                {t.memory_delete_btn}
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn load_into(
    t: &'static Translations,
    store: &MemdirStore,
    mut memories: Signal<Vec<MemoryCard>>,
    mut error: Signal<Option<String>>,
) {
    match store.scan(MAX_MEMORIES).await {
        Ok(entries) => memories.set(entries.iter().map(to_card).collect()),
        Err(err) => error.set(Some(format!("{}: {err}", t.memory_load_failed))),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::{ClearMemoryLibraryError, clear_memory_library, next_generation};

    #[test]
    fn generation_increments_without_wrapping_to_a_stale_value() {
        assert_eq!(next_generation(7), 8);
        assert_eq!(next_generation(u64::MAX), u64::MAX);
    }

    #[test]
    fn clear_library_preserves_durable_clear_when_memdir_is_unavailable() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let durable_trace = Rc::clone(&trace);
        let memdir_trace = Rc::clone(&trace);

        let result = futures::executor::block_on(clear_memory_library(
            async move {
                durable_trace.borrow_mut().push("durable");
                Ok(())
            },
            async move {
                memdir_trace.borrow_mut().push("memdir");
                Err("memdir storage is unavailable".to_string())
            },
        ));

        assert_eq!(
            result,
            Err(ClearMemoryLibraryError::Memdir(
                "memdir storage is unavailable".to_string()
            ))
        );
        assert_eq!(
            trace.borrow().as_slice(),
            ["durable", "memdir"],
            "a missing memdir must be a reported partial failure, not prevent durable clearing"
        );
    }

    #[test]
    fn clear_library_does_not_clear_memdir_after_durable_failure() {
        let result = futures::executor::block_on(clear_memory_library(
            async { Err("durable clear failed".to_string()) },
            async {
                panic!("memdir must not be cleared when durable clearing fails");
                #[allow(unreachable_code)]
                Ok(())
            },
        ));

        assert_eq!(
            result,
            Err(ClearMemoryLibraryError::Durable(
                "durable clear failed".to_string()
            ))
        );
    }
}
