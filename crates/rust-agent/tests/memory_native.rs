//! Phase 2 记忆系统集成测试（Native / redb 后端）。
//!
//! 覆盖：KvStore 读写 + TTL、Vector 索引双端契约（Native HNSW 侧）、
//! MemoryEngine 去重/淘汰/检索、DocumentStore 分块索引、memdir 可读记忆库、
//! durable 抽取与会话检查点。Web/IndexedDB 对应用例见 `tests/web_memory.rs`。

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;

use rust_agent::TokioRuntimeAdapter;
use rust_agent::error::{AgentError, MemoryError};
use rust_agent::kernel::messages::{ContentBlock, ConversationMessage, Role};
use rust_agent::memory::{
    DefaultVectorIndexManager, DocumentStore, KvStore, LocalDocumentStore, MemdirStore,
    MemoryEngine, MemoryExtractor, MemoryNamespace, MemoryType, Metric, NewMemoryEntry,
    RedbBackend, SessionCheckpoint, TABLE_DOCUMENTS, TABLE_EMBEDDINGS, TABLE_HNSW_CACHE, TABLE_KV,
    TABLE_MEMORIES, VectorIndexConfig, build_session_memory, format_iso_utc,
    load_session_checkpoint,
    memdir::{MAX_ENTRYPOINT_LINES, truncate_entrypoint},
    now_ms, parse_memory_records, save_session_checkpoint, spawn_ttl_sweeper,
};
use rust_agent::model_client::{
    EventStream, ModelClient, ModelRequest, ModelStreamEvent, UsageSnapshot,
};

const DIM: u32 = 8;

/// 确定性 embedding：按字符直方图折叠到 8 维并归一化，相似文本向量相近。
fn embed_text(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM as usize];
    for c in text.chars() {
        v[(c as usize) % DIM as usize] += 1.0;
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

struct MockModel {
    response: String,
}

#[async_trait::async_trait]
impl ModelClient for MockModel {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        let message = ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: self.response.clone(),
            }],
        };
        let events = vec![ModelStreamEvent::Complete {
            message,
            usage: UsageSnapshot::default(),
            stop_reason: None,
        }];
        Ok(futures::stream::iter(events).boxed())
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AgentError> {
        Ok(embed_text(text))
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
        Err(AgentError::Model("stt unsupported in mock".into()))
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
        Err(AgentError::Model("tts unsupported in mock".into()))
    }
}

fn test_config() -> VectorIndexConfig {
    VectorIndexConfig {
        dimension: DIM,
        distance_metric: Metric::Cosine,
        m: 16,
        ef: 50,
    }
}

fn open_backend(dir: &tempfile::TempDir) -> RedbBackend {
    RedbBackend::open(dir.path().join("ains.redb")).expect("open redb")
}

async fn build_engine(backend: &RedbBackend, namespace: MemoryNamespace) -> MemoryEngine {
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_EMBEDDINGS));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    use rust_agent::memory::VectorIndexManager;
    manager
        .create_index(namespace, test_config())
        .await
        .expect("create index");
    MemoryEngine::new(
        Arc::new(backend.table(TABLE_MEMORIES)),
        embeddings,
        Box::new(manager),
    )
}

/// 改写 memdir 条目文件里的时间戳（frontmatter 为明文，逐行替换）。
fn rewrite_entry_timestamps(raw: &str, created: &str, updated: &str) -> String {
    raw.lines()
        .map(|line| {
            if line.starts_with("created_at: ") {
                format!("created_at: {created}")
            } else if line.starts_with("updated_at: ") {
                format!("updated_at: {updated}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── 2.1 / 2.3：KvStore 读写与 TTL ──

#[tokio::test]
async fn kv_roundtrip_delete_and_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let kv = open_backend(&dir).table(TABLE_KV);

    kv.set("a/1", &json!({"v": 1}), None).await.unwrap();
    kv.set("a/2", &json!("two"), None).await.unwrap();
    kv.set("b/1", &json!(3), None).await.unwrap();

    assert_eq!(kv.get("a/1").await.unwrap(), Some(json!({"v": 1})));
    assert_eq!(kv.get("missing").await.unwrap(), None);

    let mut keys = kv.list_prefix("a/").await.unwrap();
    keys.sort();
    assert_eq!(keys, vec!["a/1".to_string(), "a/2".to_string()]);

    kv.delete("a/1").await.unwrap();
    assert_eq!(kv.get("a/1").await.unwrap(), None);
}

#[tokio::test]
async fn kv_ttl_lazy_expiry_and_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let kv = open_backend(&dir).table(TABLE_KV);

    kv.set("ttl/gone", &json!("x"), Some(Duration::from_millis(10)))
        .await
        .unwrap();
    kv.set("ttl/kept", &json!("y"), Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    // 读时检查：过期条目不可见
    assert_eq!(kv.get("ttl/gone").await.unwrap(), None);
    assert_eq!(kv.get("ttl/kept").await.unwrap(), Some(json!("y")));

    // 后台清理路径：再写一条过期数据，sweep 返回清理条数
    kv.set("ttl/gone2", &json!("z"), Some(Duration::from_millis(1)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(kv.sweep_expired().await.unwrap() >= 1);
    assert_eq!(kv.list_prefix("ttl/").await.unwrap(), vec!["ttl/kept"]);
}

#[tokio::test]
async fn ttl_sweeper_runs_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    kv.set("s/1", &json!("v"), Some(Duration::from_millis(5)))
        .await
        .unwrap();

    let handle =
        spawn_ttl_sweeper::<TokioRuntimeAdapter>(vec![Arc::clone(&kv)], Duration::from_millis(20));
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(kv.list_prefix("s/").await.unwrap().is_empty());

    handle.stop();
    assert!(handle.is_stopped());
}

// ── 2.4 / 2.5：向量索引与持久化重建 ──

#[tokio::test]
async fn vector_search_returns_closest_and_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let texts = [
        "rust borrow checker",
        "tokio async runtime",
        "coffee brewing",
    ];
    for text in texts {
        engine
            .remember(
                MemoryNamespace::Personal,
                text,
                &embed_text(text),
                json!({}),
            )
            .await
            .unwrap();
    }

    let hits = engine
        .search(
            MemoryNamespace::Personal,
            &embed_text("rust borrow checker"),
            1,
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.content, "rust borrow checker");
    assert!(hits[0].1 > 0.9);

    // 重建：新 manager 从 embeddings（Source Of Truth）加载
    drop(engine);
    let engine2 = build_engine(&backend, MemoryNamespace::Personal).await;
    assert_eq!(engine2.count(MemoryNamespace::Personal).await.unwrap(), 3);
    let hits = engine2
        .search(
            MemoryNamespace::Personal,
            &embed_text("tokio async runtime"),
            1,
        )
        .await
        .unwrap();
    assert_eq!(hits[0].0.content, "tokio async runtime");
}

// ── 2.7：去重合并 / 删除 ──

#[tokio::test]
async fn engine_dedupes_by_signature_and_merges_importance() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let first = engine
        .remember(
            MemoryNamespace::Personal,
            "User prefers dark mode.",
            &embed_text("User prefers dark mode."),
            json!({"importance": 1.0}),
        )
        .await
        .unwrap();
    // 归一化后同签名（大小写/标点差异被折叠）
    let second = engine
        .remember(
            MemoryNamespace::Personal,
            "user prefers DARK mode",
            &embed_text("user prefers DARK mode"),
            json!({"importance": 2.5}),
        )
        .await
        .unwrap();

    assert_eq!(first.id, second.id);
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
    assert_eq!(second.metadata["importance"], json!(2.5));
    // 基线刷新语义：正文以新写入为准（SoT 与新向量不分叉）
    assert_eq!(second.content, "user prefers DARK mode");
    let stored = engine
        .get(MemoryNamespace::Personal, &first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "user prefers DARK mode");

    engine
        .forget(MemoryNamespace::Personal, &first.id)
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 0);
    assert!(
        engine
            .search(MemoryNamespace::Personal, &embed_text("dark mode"), 3)
            .await
            .unwrap()
            .is_empty()
    );
}

// ── 2.6：DocumentStore 分块索引与检索 ──

#[tokio::test]
async fn document_store_index_search_delete() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Document).await;
    let model: Arc<dyn ModelClient> = Arc::new(MockModel {
        response: String::new(),
    });
    let mut store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        Arc::clone(&model),
    );

    let content = "# Setup\n\nInstall rustup and the stable toolchain.\n\n# Testing\n\nRun cargo test for the whole workspace.";
    let meta = store.index_content("guide.md", content).await.unwrap();
    assert!(meta.chunk_count >= 1);
    assert!(store.is_indexed(&meta.source_hash).await.unwrap());

    // 重复索引：source_hash 命中，直接返回既有 meta
    let again = store.index_content("guide.md", content).await.unwrap();
    assert_eq!(again.id, meta.id);
    assert_eq!(store.list_docs().await.unwrap().len(), 1);

    let results = store.search("cargo test workspace", 2, None).await.unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].doc_name, "guide.md");

    store.delete(&meta.id).await.unwrap();
    assert!(store.list_docs().await.unwrap().is_empty());
    assert!(!store.is_indexed(&meta.source_hash).await.unwrap());
}

// ── 2.6：DocumentStore search 限定 doc_ids（跨文档对比场景，过采样 + 过滤）──

#[tokio::test]
async fn document_search_filters_by_doc_ids() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Document).await;
    let model: Arc<dyn ModelClient> = Arc::new(MockModel {
        response: String::new(),
    });
    let mut store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        Arc::clone(&model),
    );

    let a = store
        .index_content("apple.md", "apple orchard harvest notes")
        .await
        .unwrap();
    let b = store
        .index_content("banana.md", "banana plantation shipping notes")
        .await
        .unwrap();
    assert_ne!(a.id, b.id);

    // 不限定：两个文档都可能命中
    let all = store.search("notes", 10, None).await.unwrap();
    let names: std::collections::HashSet<&str> = all.iter().map(|r| r.doc_name.as_str()).collect();
    assert!(
        names.contains("apple.md") && names.contains("banana.md"),
        "unfiltered search should reach both docs: {names:?}"
    );

    // 限定 doc_ids = [a]：过采样后过滤，结果只含 A 文档的 chunk
    let only_a = store
        .search("notes", 10, Some(std::slice::from_ref(&a.id)))
        .await
        .unwrap();
    assert!(!only_a.is_empty());
    assert!(
        only_a
            .iter()
            .all(|r| r.chunk.doc_id == a.id && r.doc_name == "apple.md"),
        "doc_ids filter leaked other docs: {only_a:?}"
    );

    // 限定不存在的 doc_id：空结果
    let none = store
        .search("notes", 10, Some(&["doc-missing".to_string()]))
        .await
        .unwrap();
    assert!(none.is_empty());
}

// ── 2.8：memdir 可读记忆库 ──

#[tokio::test]
async fn memdir_add_scan_remove_and_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(Arc::clone(&kv));

    // 空库：提示词包含策略行与占位
    let prompt = store.load_memory_prompt().await.unwrap();
    assert!(prompt.contains("## Durable memory policy"));
    assert!(prompt.contains("(not created yet)"));

    let filename = store
        .add_entry(NewMemoryEntry {
            title: "Build Setup".into(),
            body: "Use `cargo build -p rust-agent` for the runtime crate.".into(),
            description: "How to build the runtime".into(),
            memory_type: MemoryType::Project,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(filename, "build_setup.md");

    let index = store.read_index().await.unwrap().unwrap();
    assert!(index.contains("- [Build Setup](build_setup.md)"));

    // 同签名去重：返回既有文件名，不重复建条目；基线刷新语义：
    // 标题/正文以新写入为准，索引行同步更新为新标题（upsert），
    // 避免 MEMORY.md 索引标题与条目 frontmatter 的 name 不一致。
    let dup = store
        .add_entry(NewMemoryEntry {
            title: "Build Setup Again".into(),
            body: "use CARGO BUILD -p rust-agent for the runtime crate".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(dup, filename);

    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "Build Setup Again");
    assert!(entries[0].body.contains("CARGO BUILD"));
    assert!(entries[0].id.starts_with("mem-"));

    // 刷新后的索引行标题必须与条目一致（P2 回归）
    let index = store.read_index().await.unwrap().unwrap();
    assert!(!index.contains("[Build Setup]"));
    assert!(index.contains("- [Build Setup Again](build_setup.md)"));

    let prompt = store.load_memory_prompt().await.unwrap();
    assert!(prompt.contains("```md"));
    assert!(prompt.contains("build_setup.md"));

    // 软删除：scan 过滤 + 索引行移除，重复删除返回 false
    assert!(store.remove_entry("build_setup").await.unwrap());
    assert!(store.scan(10).await.unwrap().is_empty());
    assert!(
        !store
            .read_index()
            .await
            .unwrap()
            .unwrap()
            .contains("build_setup.md")
    );
    assert!(!store.remove_entry("build_setup").await.unwrap());
}

#[tokio::test]
async fn memdir_clear_entries_hides_all_entries_and_clears_index() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let store = MemdirStore::new(Arc::new(backend.table(TABLE_KV)));
    for title in ["Build", "Deploy"] {
        store
            .add_entry(NewMemoryEntry {
                title: title.to_string(),
                description: format!("{title} workflow"),
                body: format!("{title} steps"),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    assert_eq!(store.clear_entries().await.unwrap(), 2);
    assert!(store.scan(20).await.unwrap().is_empty());
    assert_eq!(store.read_index().await.unwrap(), None);
    // 已清空时应幂等，不能重复计数。
    assert_eq!(store.clear_entries().await.unwrap(), 0);
}

// ── Fix 回归：clear_entries 连同损坏行一并清除（L4 收敛）──

#[tokio::test]
async fn memdir_clear_entries_removes_corrupt_rows_too() {
    let dir = tempfile::tempdir().unwrap();
    // 先写入一条合法条目。
    {
        let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
        let store = MemdirStore::new(kv);
        store
            .add_entry(NewMemoryEntry {
                title: "Build".into(),
                body: "original build body".into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // 再注入一条损坏行（Envelope 合法但 JSON 载荷非法）。
    {
        let payload = b"not jso";
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(payload);
        use redb::TableDefinition;
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(TABLE_KV);
        let db = redb::Database::open(dir.path().join("ains.redb")).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(def).unwrap();
            table
                .insert("memdir/entries/broken.md", bytes.as_slice())
                .unwrap();
        }
        write.commit().unwrap();
    }
    // clear 应把损坏行一并清除，计数包含损坏条目，索引同步清空。
    {
        let kv: Arc<dyn KvStore> = Arc::new(
            RedbBackend::open(dir.path().join("ains.redb"))
                .unwrap()
                .table(TABLE_KV),
        );
        let store = MemdirStore::new(kv);
        assert_eq!(
            store.clear_entries().await.unwrap(),
            2,
            "legal + corrupt rows must all be removed"
        );
        assert!(store.scan(20).await.unwrap().is_empty());
        assert_eq!(store.read_index().await.unwrap(), None);
        // 清空后可正常重新写入（索引重建，无残留空行）。
        store
            .add_entry(NewMemoryEntry {
                title: "Rebuilt".into(),
                body: "fresh body".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let entries = store.scan(20).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Rebuilt");
        assert_eq!(
            store.read_index().await.unwrap().unwrap(),
            "- [Rebuilt](rebuilt.md)\n"
        );
    }
}

#[tokio::test]
async fn memdir_exact_id_delete_cannot_match_another_entries_title() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    store
        .add_entry(NewMemoryEntry {
            title: "Target".into(),
            body: "target body".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let target_id = store.scan(10).await.unwrap()[0].id.clone();
    store
        .add_entry(NewMemoryEntry {
            // The UI passes `target_id`; a fuzzy deletion API must not select
            // this separate record merely because its title happens to match.
            title: target_id.clone(),
            body: "different body".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(store.delete_entry_by_id(&target_id).await.unwrap());
    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, target_id);
}

#[tokio::test]
async fn scoped_memdir_isolates_entries_between_owners() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let owner_a = MemdirStore::new_scoped(Arc::clone(&kv), "owner-a-hash");
    let owner_b = MemdirStore::new_scoped(kv, "owner-b-hash");
    owner_a
        .add_entry(NewMemoryEntry {
            title: "Account A note".into(),
            body: "account A private durable note".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(owner_b.scan(10).await.unwrap().is_empty());
    assert!(owner_b.read_index().await.unwrap().is_none());
}

#[test]
fn memdir_entrypoint_truncation() {
    let long_lines = "line\n".repeat(300);
    let (text, reason) = truncate_entrypoint(&long_lines);
    assert_eq!(text.lines().count(), MAX_ENTRYPOINT_LINES);
    assert_eq!(reason.as_deref(), Some("300 lines (limit: 200)"));

    let big = format!("{}\n", "x".repeat(30_000));
    let (_, reason) = truncate_entrypoint(&big);
    assert_eq!(reason.as_deref(), Some("30001 bytes (limit: 25000)"));

    let (text, reason) = truncate_entrypoint("short\n");
    assert_eq!(text, "short\n");
    assert!(reason.is_none());
}

// ── 2.9：durable 抽取与会话检查点 ──

#[tokio::test]
async fn extractor_gates_and_saves_records() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let response = r#"Here you go:
{"memories": [
  {"title": "Deploy target", "content": "Production deploys run from the main branch only.", "type": "project", "scope": "team"},
  {"title": "User preference", "content": "The user prefers concise answers.", "type": "user", "scope": "personal"}
]}"#;
    let extractor = MemoryExtractor::new(
        MemdirStore::new(Arc::clone(&kv)),
        Arc::new(MockModel {
            response: response.to_string(),
        }),
    );

    let messages = vec![
        ConversationMessage::from_user_text("deploy only from main"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Understood.".into(),
            }],
        },
    ];

    // gating：消息不足 / 本会话已写过记忆
    let outcome = extractor
        .maybe_extract(&messages[..1], false)
        .await
        .unwrap();
    assert_eq!(outcome.skipped.as_deref(), Some("not enough messages"));
    let outcome = extractor.maybe_extract(&messages, true).await.unwrap();
    assert!(outcome.skipped.is_some());

    let outcome = extractor.maybe_extract(&messages, false).await.unwrap();
    assert!(outcome.skipped.is_none());
    assert_eq!(outcome.saved.len(), 2);

    let store = MemdirStore::new(kv);
    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 2);
}

#[test]
fn parse_memory_records_is_lenient() {
    assert!(parse_memory_records("no json here").is_empty());
    assert!(parse_memory_records("{\"memories\": []}").is_empty());

    let records = parse_memory_records(
        "prefix {\"memories\": [{\"content\": \"fact one\", \"scope\": \"shared\"}]} suffix",
    );
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].body, "fact one");
    assert_eq!(records[0].scope.as_str(), "team");
    // 缺 title 时回落 body 截断
    assert_eq!(records[0].title, "fact one");
}

#[tokio::test]
async fn session_checkpoint_roundtrip_and_truncation() {
    let checkpoint = SessionCheckpoint {
        current_state: "Implementing Phase 2 memory".into(),
        next_step: Some("Write alignment doc".into()),
        verified_work: vec!["kv tests pass".into()],
        active_artifacts: (0..15).map(|i| format!("file{i}.rs")).collect(),
    };
    let messages = vec![
        ConversationMessage::from_user_text("continue phase 2"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Working on memdir.".into(),
            }],
        },
    ];
    let doc = build_session_memory(&checkpoint, &messages);
    assert!(doc.starts_with("# Session Memory"));
    assert!(doc.contains("## Current State"));
    assert!(doc.contains("## Next Step"));
    // Active Artifacts 只保留最后 10 个
    assert!(!doc.contains("file4.rs"));
    assert!(doc.contains("file14.rs"));
    assert!(doc.contains("- user: continue phase 2"));

    // 超预算截断
    let huge = SessionCheckpoint {
        current_state: "s".repeat(20_000),
        ..Default::default()
    };
    let doc = build_session_memory(&huge, &[]);
    assert!(doc.chars().count() <= 12_000);
    assert!(doc.ends_with("> Session memory was truncated to stay within budget.\n"));

    // 持久化 roundtrip
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    save_session_checkpoint(&kv, "# Session Memory\ntest")
        .await
        .unwrap();
    assert_eq!(
        load_session_checkpoint(&kv).await.unwrap().as_deref(),
        Some("# Session Memory\ntest")
    );
}

// ── Review 回归：H1 索引行锚定匹配 ──

#[tokio::test]
async fn memdir_index_lines_are_anchored_on_filename() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    // web_build.md 先入索引；build.md 是其子串，锚定后不应被误判为已存在
    let f1 = store
        .add_entry(NewMemoryEntry {
            title: "Web Build".into(),
            body: "wasm-pack build for the web target".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(f1, "web_build.md");
    let f2 = store
        .add_entry(NewMemoryEntry {
            title: "Build".into(),
            body: "cargo build for the native target".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(f2, "build.md");

    let index = store.read_index().await.unwrap().unwrap();
    assert!(index.contains("(web_build.md)"));
    assert!(index.contains("(build.md)"));

    // 删除 build.md 只应移除自己的索引行
    assert!(store.remove_entry("build").await.unwrap());
    let index = store.read_index().await.unwrap().unwrap();
    assert!(index.contains("(web_build.md)"));
    assert!(!index.contains("(build.md)"));
}

#[tokio::test]
async fn memdir_append_index_sanitizes_title_like_upsert() {
    // 修复回归（review P3）：append 路径（新条目）与 upsert 刷新路径
    // 必须用同一清洗口径，否则标题含换行/`]` 会破坏 `- [title](file)`
    // 索引行的 markdown 链接结构（换行分裂多行、`]` 提前闭合链接）。
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    let filename = store
        .add_entry(NewMemoryEntry {
            title: "Build\nnotes [v1]".into(),
            body: "body".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let index = store.read_index().await.unwrap().unwrap();
    // 换行与方括号被剥除，索引保持单行、链接结构完好
    assert_eq!(
        index,
        format!("- [Buildnotes v1]({filename})\n"),
        "append index line must sanitize title exactly like upsert"
    );
    assert_eq!(
        index.lines().count(),
        1,
        "title newline must not split index"
    );

    // 相同 body 再 add 走刷新（upsert）路径：与 append 同口径清洗，
    // 且不产生重复索引行（slugify("Build\nnotes [v1]") == "build_notes_v1"）
    let updated = store
        .add_entry(NewMemoryEntry {
            title: "Build\nnotes [v1]".into(),
            body: "body".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(updated, filename);
    let index = store.read_index().await.unwrap().unwrap();
    assert_eq!(index.matches("(build_notes_v1.md)").count(), 1);
}

// ── Review 回归：H2 软删除后 re-add 恢复索引行；M3 同名条目不被禁用项遮蔽 ──

#[tokio::test]
async fn memdir_readd_after_remove_restores_index_line() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    let entry = NewMemoryEntry {
        title: "Deploy Steps".into(),
        body: "Deploy from main branch only.".into(),
        ..Default::default()
    };
    let filename = store.add_entry(entry.clone()).await.unwrap();
    assert!(store.remove_entry("deploy_steps").await.unwrap());
    assert!(store.scan(10).await.unwrap().is_empty());

    // 同签名 re-add：刷新既有文件、恢复启用与索引行
    let again = store.add_entry(entry).await.unwrap();
    assert_eq!(again, filename);
    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].disabled);
    let index = store.read_index().await.unwrap().unwrap();
    assert_eq!(index.matches("(deploy_steps.md)").count(), 1);
}

#[tokio::test]
async fn memdir_remove_same_title_twice() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    for body in ["first note body", "second note body"] {
        store
            .add_entry(NewMemoryEntry {
                title: "Note".into(),
                body: body.into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    assert_eq!(store.scan(10).await.unwrap().len(), 2);

    // 已禁用的同名条目不应遮蔽仍启用的另一条
    assert!(store.remove_entry("Note").await.unwrap());
    assert!(store.remove_entry("Note").await.unwrap());
    assert!(!store.remove_entry("Note").await.unwrap());
    assert!(store.scan(10).await.unwrap().is_empty());
}

// ── Review 回归：M1 去重刷新更新新鲜度 ──

#[tokio::test]
async fn remember_refresh_updates_recency() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    engine
        .remember(
            MemoryNamespace::Personal,
            "prefers rust",
            &embed_text("prefers rust"),
            json!({}),
        )
        .await
        .unwrap();
    let refreshed = engine
        .remember(
            MemoryNamespace::Personal,
            "prefers rust",
            &embed_text("prefers rust"),
            json!({}),
        )
        .await
        .unwrap();
    assert!(refreshed.metadata.get("refreshed_at").is_some());
}

#[test]
fn retention_score_uses_refreshed_at() {
    use rust_agent::memory::manage::{effective_recency_ms, retention_score};

    let now: i64 = 1_753_600_000_000;
    let sixty_days_ago = now - 60 * 24 * 3600 * 1000;

    // 无 refreshed_at：60 天 → 两个半衰期 ≈ 0.25
    let stale = retention_score(&json!({"importance": 1.0}), sixty_days_ago, now);
    assert!((stale - 0.25).abs() < 0.01);

    // 刚刷新过：按 refreshed_at 计算 → ≈ 1.0
    let fresh = retention_score(
        &json!({"importance": 1.0, "refreshed_at": now}),
        sixty_days_ago,
        now,
    );
    assert!((fresh - 1.0).abs() < 0.01);

    assert_eq!(
        effective_recency_ms(&json!({"refreshed_at": now}), sixty_days_ago),
        now
    );
    assert_eq!(
        effective_recency_ms(&json!({}), sixty_days_ago),
        sixty_days_ago
    );
}

// ── 2.7 记忆管理：search_ranked 时间衰减重排（旧记忆降排名，可翻转原始名次）──

#[tokio::test]
async fn search_ranked_reranks_by_time_decay() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    // old 向量与 query 完全对齐（原始余弦 1.0）但 60 天前刷新（两个半衰期
    // → 衰减 0.25）；new 向量偏离（余弦 ≈ 0.707）但刚刷新（衰减 ≈ 1.0）。
    // 原始名次 old > new，衰减后应翻转为 new > old。
    let now = rust_agent::memory::now_ms();
    let sixty_days = 60 * 24 * 3600 * 1000;
    let query = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let inv = 1.0f32 / 2.0f32.sqrt();
    let new_vec = vec![inv, inv, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    engine
        .insert_with_id(
            MemoryNamespace::Personal,
            "old",
            "stale memory",
            &query,
            json!({ "refreshed_at": now - sixty_days }),
        )
        .await
        .unwrap();
    engine
        .insert_with_id(
            MemoryNamespace::Personal,
            "new",
            "fresh memory",
            &new_vec,
            json!({ "refreshed_at": now }),
        )
        .await
        .unwrap();

    // 原始检索：old 余弦更高排在前，旧条目未被惩罚
    let plain = engine
        .search(MemoryNamespace::Personal, &query, 2)
        .await
        .unwrap();
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[0].0.id, "old", "raw ranking: exact match first");
    assert!(plain[0].1 > 0.99);

    // 衰减重排：新鲜条目翻转到最前，旧条目分数被衰减压低
    let ranked = engine
        .search_ranked(MemoryNamespace::Personal, &query, 2)
        .await
        .unwrap();
    assert_eq!(ranked.len(), 2);
    assert_eq!(
        ranked[0].0.id, "new",
        "decay must promote fresh entry above stale exact match"
    );
    assert_eq!(ranked[1].0.id, "old");
    assert!(ranked[0].1 > ranked[1].1);
    // old ≈ 1.0 × 0.5^(60/30) = 0.25
    assert!(
        ranked[1].1 < 0.5,
        "stale score must be decayed: {}",
        ranked[1].1
    );
}

// ── Review 回归：M4 损坏行不毒化 list_prefix / sweep ──

#[tokio::test]
async fn corrupt_row_does_not_poison_scans() {
    let dir = tempfile::tempdir().unwrap();
    {
        let backend = open_backend(&dir);
        let kv = backend.table(TABLE_KV);
        kv.set("m/good", &json!("ok"), None).await.unwrap();
    }

    // 绕过 Envelope，直接写入无法解码的裸字节（redb 独占锁：需先释放上面的句柄）
    {
        use redb::TableDefinition;
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(TABLE_KV);
        let db = redb::Database::open(dir.path().join("ains.redb")).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(def).unwrap();
            table.insert("m/corrupt", [0xFFu8; 3].as_slice()).unwrap();
        }
        write.commit().unwrap();
    }

    let kv = RedbBackend::open(dir.path().join("ains.redb"))
        .unwrap()
        .table(TABLE_KV);
    let keys = kv.list_prefix("m/").await.unwrap();
    assert_eq!(keys, vec!["m/good"]);
    // sweep 不报错、不误删损坏行
    assert_eq!(kv.sweep_expired().await.unwrap(), 0);
}

// ── Review 回归：L1/L2 frontmatter 特殊字符 roundtrip ──

#[tokio::test]
async fn memdir_frontmatter_special_chars_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    let title = "Build: \"Setup\" Guide";
    let description = "line one\nhas --- dashes\tand \\backslash";
    store
        .add_entry(NewMemoryEntry {
            title: title.into(),
            body: "Special char roundtrip body.".into(),
            description: description.into(),
            tags: vec!["a:b".into(), "#hash".into(), "- dash".into()],
            ..Default::default()
        })
        .await
        .unwrap();

    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.name, title);
    assert_eq!(entry.description, description);
    assert_eq!(entry.tags, vec!["a:b", "#hash", "- dash"]);
    assert_eq!(entry.body.trim(), "Special char roundtrip body.");
}

// ── Review 回归：N2/N3 YAML 原生标量与控制字符 roundtrip ──

#[tokio::test]
async fn memdir_yaml_native_scalars_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    // "true"/"1.5"/"no" 未加引号会被 YAML 解析为 bool/number，
    // 必须 roundtrip 为字符串
    for (title, description, body) in [
        ("true", "no", "yaml native scalar bool title"),
        ("1.5", "42", "yaml native scalar number title"),
        ("Ctrl", "has \u{1} control char", "control char description"),
    ] {
        store
            .add_entry(NewMemoryEntry {
                title: title.into(),
                body: body.into(),
                description: description.into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    let mut entries = store.scan(10).await.unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].name, "1.5");
    assert_eq!(entries[0].description, "42");
    assert_eq!(entries[1].name, "Ctrl");
    assert_eq!(entries[1].description, "has \u{1} control char");
    assert_eq!(entries[2].name, "true");
    assert_eq!(entries[2].description, "no");
}

// ── Fix 回归：B1 加载韧性 + 非有限向量拒绝 ──

#[tokio::test]
async fn vector_load_skips_corrupt_embedding_rows() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;
    for text in ["alpha fact", "beta note"] {
        engine
            .remember(
                MemoryNamespace::Personal,
                text,
                &embed_text(text),
                json!({}),
            )
            .await
            .unwrap();
    }

    // 注入损坏 embedding 行：维度不符 / 含 null / 非数值分量
    let embeddings = backend.table(TABLE_EMBEDDINGS);
    embeddings
        .set("personal/bad-dim", &json!([1.0, 2.0]), None)
        .await
        .unwrap();
    embeddings
        .set(
            "personal/bad-null",
            &json!([null, 0, 0, 0, 0, 0, 0, 0]),
            None,
        )
        .await
        .unwrap();
    embeddings
        .set(
            "personal/bad-type",
            &json!(["a", "b", "c", "d", "e", "f", "g", "h"]),
            None,
        )
        .await
        .unwrap();
    drop(engine);

    // 旧行为：load 遇到首个坏行整体报错；现在跳过坏行、有效数据可检索
    let engine2 = build_engine(&backend, MemoryNamespace::Personal).await;
    let hits = engine2
        .search(MemoryNamespace::Personal, &embed_text("alpha fact"), 2)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.content, "alpha fact");
}

#[tokio::test]
async fn engine_rejects_non_finite_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let mut bad = embed_text("nan vector");
    bad[0] = f32::NAN;
    assert!(
        engine
            .remember(MemoryNamespace::Personal, "nan vector", &bad, json!({}))
            .await
            .is_err()
    );
    bad[0] = f32::INFINITY;
    assert!(
        engine
            .insert_with_id(
                MemoryNamespace::Personal,
                "inf-1",
                "inf vector",
                &bad,
                json!({})
            )
            .await
            .is_err()
    );
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 0);
}

// ── Fix 回归：forget 签名守卫 ──

#[tokio::test]
async fn forget_preserves_signature_owned_by_other_entry() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let content = "User prefers tabs over spaces.";
    let first = engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();
    // 相同内容以自定义 id 写入（insert_with_id 不登记签名）
    engine
        .insert_with_id(
            MemoryNamespace::Personal,
            "custom-1",
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();
    engine
        .forget(MemoryNamespace::Personal, "custom-1")
        .await
        .unwrap();

    // 签名仍归属 first：再次 remember 走去重刷新而非新建
    let again = engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
}

// ── Fix 回归：去重刷新同步替换向量 ──

#[tokio::test]
async fn dedupe_refresh_replaces_stale_vector() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    // 两文本归一化签名相同（大小写/标点折叠），但 embedding 显著不同
    let old_text = "ALPHA BRAVO";
    let new_text = "alpha!!! bravo???";
    engine
        .remember(
            MemoryNamespace::Personal,
            old_text,
            &embed_text(old_text),
            json!({}),
        )
        .await
        .unwrap();
    engine
        .remember(
            MemoryNamespace::Personal,
            new_text,
            &embed_text(new_text),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);

    // 索引应持有刷新后的向量：新向量精确命中（旧行为分数 ≈ 0.90）
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text(new_text), 1)
        .await
        .unwrap();
    assert!(
        hits[0].1 > 0.999,
        "stale vector still in index: {}",
        hits[0].1
    );
    // 内容同步替换为新写入（与新向量不分叉）
    assert_eq!(hits[0].0.content, new_text);

    // Source Of Truth（embeddings）也已替换：重启重建后仍精确命中
    drop(engine);
    let engine2 = build_engine(&backend, MemoryNamespace::Personal).await;
    let hits = engine2
        .search(MemoryNamespace::Personal, &embed_text(new_text), 1)
        .await
        .unwrap();
    assert!(hits[0].1 > 0.999);
}

// ── Fix 回归：容量策略（remember 淘汰 / insert_with_id 报错）──

#[tokio::test]
async fn capacity_full_remember_evicts_but_insert_with_id_errors() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal)
        .await
        .with_max_entries(2);

    for text in ["entry one", "entry two"] {
        engine
            .remember(
                MemoryNamespace::Personal,
                text,
                &embed_text(text),
                json!({}),
            )
            .await
            .unwrap();
    }
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 2);

    // 文档 chunk 类写入：满则报错，不静默淘汰
    let err = engine
        .insert_with_id(
            MemoryNamespace::Personal,
            "chunk-1",
            "doc chunk",
            &embed_text("doc chunk"),
            json!({}),
        )
        .await;
    assert!(err.is_err());
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 2);

    // 个人记忆写入：满则按保留权重淘汰一条后写入
    engine
        .remember(
            MemoryNamespace::Personal,
            "entry three",
            &embed_text("entry three"),
            json!({"importance": 5.0}),
        )
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 2);
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text("entry three"), 2)
        .await
        .unwrap();
    assert!(hits.iter().any(|(e, _)| e.content == "entry three"));
}

#[tokio::test]
async fn capacity_eviction_never_deletes_another_dedupe_domain() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal)
        .await
        .with_max_entries(1);

    let owner_a = engine
        .remember_in_domain(
            MemoryNamespace::Personal,
            "personal:private:owner-a",
            "owner A memory",
            &embed_text("owner A memory"),
            json!({}),
        )
        .await
        .unwrap();

    // The shared physical Personal namespace is full, but owner B has no
    // entry in its own dedupe domain.  It must fail rather than evict owner A.
    let error = engine
        .remember_in_domain(
            MemoryNamespace::Personal,
            "personal:private:owner-b",
            "owner B memory",
            &embed_text("owner B memory"),
            json!({}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, MemoryError::Storage(ref message) if message.contains("other isolated domains")),
        "unexpected capacity error: {error:?}"
    );
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
    assert!(
        engine
            .get(MemoryNamespace::Personal, &owner_a.id)
            .await
            .unwrap()
            .is_some(),
        "owner A memory must survive owner B's capacity attempt"
    );
}

#[tokio::test]
async fn insert_with_id_updates_existing_entry_when_namespace_is_full() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal)
        .await
        .with_max_entries(1);

    engine
        .insert_with_id(
            MemoryNamespace::Personal,
            "stable-id",
            "old content",
            &embed_text("old content"),
            json!({"revision": 1}),
        )
        .await
        .unwrap();

    engine
        .insert_with_id(
            MemoryNamespace::Personal,
            "stable-id",
            "new content",
            &embed_text("new content"),
            json!({"revision": 2}),
        )
        .await
        .unwrap();

    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
    let entry = engine
        .get(MemoryNamespace::Personal, "stable-id")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.content, "new content");
    assert_eq!(entry.metadata["revision"], 2);
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text("new content"), 1)
        .await
        .unwrap();
    assert_eq!(hits[0].0.id, "stable-id");
    assert_eq!(hits[0].0.content, "new content");
}

// ── Fix 回归：clear_namespace 全量清理（SoT + 签名 + 索引）──

#[tokio::test]
async fn clear_namespace_purges_sot_signatures_and_index() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let content = "clearable fact";
    engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();
    engine
        .clear_namespace(MemoryNamespace::Personal)
        .await
        .unwrap();

    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 0);
    let memories = backend.table(TABLE_MEMORIES);
    assert!(memories.list_prefix("personal/").await.unwrap().is_empty());
    assert!(
        memories
            .list_prefix("sig/personal/")
            .await
            .unwrap()
            .is_empty()
    );
    let embeddings = backend.table(TABLE_EMBEDDINGS);
    assert!(
        embeddings
            .list_prefix("personal/")
            .await
            .unwrap()
            .is_empty()
    );

    // 索引实例已移除；重建后可继续写入，签名已清理 → 新建条目
    engine
        .vector
        .create_index(MemoryNamespace::Personal, test_config())
        .await
        .unwrap();
    engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
}

// ── Fix 回归：超大 TTL 饱和而非回绕 ──

#[tokio::test]
async fn huge_ttl_saturates_instead_of_wrapping() {
    let dir = tempfile::tempdir().unwrap();
    let kv = open_backend(&dir).table(TABLE_KV);
    kv.set("ttl/huge", &json!("v"), Some(Duration::from_secs(u64::MAX)))
        .await
        .unwrap();
    // 旧行为：`as i64` 截断可得负的过期时刻 → 条目立即“过期”
    assert_eq!(kv.get("ttl/huge").await.unwrap(), Some(json!("v")));
    assert_eq!(kv.list_prefix("ttl/").await.unwrap(), vec!["ttl/huge"]);
}

// ── Fix 回归：B2 文档索引中途失败回收孤儿 chunk ──

struct FlakyEmbedModel {
    fail_after: usize,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ModelClient for FlakyEmbedModel {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        Err(AgentError::Model("unused".into()))
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AgentError> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n >= self.fail_after {
            return Err(AgentError::Model("embed backend down".into()));
        }
        Ok(embed_text(text))
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
        Err(AgentError::Model("stt unsupported in mock".into()))
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
        Err(AgentError::Model("tts unsupported in mock".into()))
    }
}

#[tokio::test]
async fn document_index_failure_cleans_up_partial_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Document).await;
    let model: Arc<dyn ModelClient> = Arc::new(FlakyEmbedModel {
        fail_after: 1,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        Arc::clone(&model),
    );

    // 两个标题 → 两个 chunk；第一个 embed 成功、第二个失败
    let content = "# Setup\n\nInstall rustup and the stable toolchain.\n\n# Testing\n\nRun cargo test for the workspace.";
    assert!(store.index_content("guide.md", content).await.is_err());

    // 已插入的 chunk 全部回收，无孤儿数据、无 meta
    let memories = backend.table(TABLE_MEMORIES);
    assert!(memories.list_prefix("document/").await.unwrap().is_empty());
    let embeddings = backend.table(TABLE_EMBEDDINGS);
    assert!(
        embeddings
            .list_prefix("document/")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.list_docs().await.unwrap().is_empty());
}

// ── Fix 回归：KvStore 写入故障注入（指定第 N 次 set 失败，1-based，可多点）──

struct FailingKv {
    inner: Box<dyn KvStore>,
    fail_on: Vec<usize>,
    sets: std::sync::atomic::AtomicUsize,
}

impl FailingKv {
    fn new(inner: impl KvStore + 'static, fail_on_set: usize) -> Self {
        Self::failing_at(inner, vec![fail_on_set])
    }

    fn failing_at(inner: impl KvStore + 'static, fail_on: Vec<usize>) -> Self {
        Self {
            inner: Box::new(inner),
            fail_on,
            sets: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl KvStore for FailingKv {
    async fn get(&self, key: &str) -> Result<Option<serde_json::Value>, MemoryError> {
        self.inner.get(key).await
    }

    async fn set(
        &self,
        key: &str,
        value: &serde_json::Value,
        ttl: Option<Duration>,
    ) -> Result<(), MemoryError> {
        let n = self.sets.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if self.fail_on.contains(&n) {
            return Err(MemoryError::Storage("injected set failure".into()));
        }
        self.inner.set(key, value, ttl).await
    }

    async fn delete(&self, key: &str) -> Result<(), MemoryError> {
        self.inner.delete(key).await
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
        self.inner.list_prefix(prefix).await
    }
}

/// Test index manager that mutates its resident vector before reporting the
/// second add as failed.  It models a backend that cannot atomically roll back
/// an in-memory graph update on its own.
struct MutatingThenFailingVectorManager {
    vectors: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<f32>>>>,
    adds: usize,
    fail_on_add: usize,
}

#[async_trait::async_trait]
impl rust_agent::memory::VectorIndexManager for MutatingThenFailingVectorManager {
    async fn create_index(
        &mut self,
        _namespace: MemoryNamespace,
        _config: VectorIndexConfig,
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    async fn remove_index(&mut self, _namespace: MemoryNamespace) -> Result<(), MemoryError> {
        self.vectors.lock().unwrap().clear();
        Ok(())
    }

    async fn add(
        &mut self,
        namespace: MemoryNamespace,
        node_id: &str,
        vector: &[f32],
    ) -> Result<(), MemoryError> {
        self.adds += 1;
        self.vectors
            .lock()
            .unwrap()
            .insert(namespace.storage_key(node_id), vector.to_vec());
        if self.adds == self.fail_on_add {
            Err(MemoryError::Storage(
                "injected post-mutation index failure".into(),
            ))
        } else {
            Ok(())
        }
    }

    async fn remove(
        &mut self,
        namespace: MemoryNamespace,
        node_id: &str,
    ) -> Result<(), MemoryError> {
        self.vectors
            .lock()
            .unwrap()
            .remove(&namespace.storage_key(node_id));
        Ok(())
    }

    async fn search(
        &self,
        _namespace: MemoryNamespace,
        _query: &[f32],
        _top_k: usize,
    ) -> Result<Vec<(String, f32)>, MemoryError> {
        Ok(Vec::new())
    }
}

/// 第一次 remove 注入失败，第二次成功；用于验证失败时 durable rows 仍保留，
/// 从而让调用方能够按同一 id 重试清理。
struct FailOnceRemoveVectorManager {
    vectors: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<f32>>>>,
    fail_next_remove: bool,
    namespace_missing_on_remove: bool,
}

#[async_trait::async_trait]
impl rust_agent::memory::VectorIndexManager for FailOnceRemoveVectorManager {
    async fn create_index(
        &mut self,
        _namespace: MemoryNamespace,
        _config: VectorIndexConfig,
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    async fn remove_index(&mut self, _namespace: MemoryNamespace) -> Result<(), MemoryError> {
        self.vectors.lock().unwrap().clear();
        Ok(())
    }

    async fn add(
        &mut self,
        namespace: MemoryNamespace,
        node_id: &str,
        vector: &[f32],
    ) -> Result<(), MemoryError> {
        self.vectors
            .lock()
            .unwrap()
            .insert(namespace.storage_key(node_id), vector.to_vec());
        Ok(())
    }

    async fn remove(
        &mut self,
        namespace: MemoryNamespace,
        node_id: &str,
    ) -> Result<(), MemoryError> {
        if self.namespace_missing_on_remove {
            return Err(MemoryError::NamespaceNotFound(namespace));
        }
        if self.fail_next_remove {
            self.fail_next_remove = false;
            return Err(MemoryError::Storage(
                "injected vector remove failure".into(),
            ));
        }
        self.vectors
            .lock()
            .unwrap()
            .remove(&namespace.storage_key(node_id));
        Ok(())
    }

    async fn search(
        &self,
        _namespace: MemoryNamespace,
        _query: &[f32],
        _top_k: usize,
    ) -> Result<Vec<(String, f32)>, MemoryError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn forget_keeps_durable_rows_when_vector_remove_fails_for_retry() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let vectors = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut engine = MemoryEngine::new(
        Arc::new(backend.table(TABLE_MEMORIES)),
        Arc::new(backend.table(TABLE_EMBEDDINGS)),
        Box::new(FailOnceRemoveVectorManager {
            vectors: Arc::clone(&vectors),
            fail_next_remove: true,
            namespace_missing_on_remove: false,
        }),
    );

    let entry = engine
        .remember(
            MemoryNamespace::Personal,
            "retryable forced-delete vector",
            &[1.0; DIM as usize],
            json!({}),
        )
        .await
        .unwrap();
    let key = MemoryNamespace::Personal.storage_key(&entry.id);

    assert!(
        engine
            .forget(MemoryNamespace::Personal, &entry.id)
            .await
            .is_err()
    );
    assert!(
        engine
            .get(MemoryNamespace::Personal, &entry.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        backend
            .table(TABLE_EMBEDDINGS)
            .get(&key)
            .await
            .unwrap()
            .is_some()
    );

    engine
        .forget(MemoryNamespace::Personal, &entry.id)
        .await
        .unwrap();
    assert!(
        engine
            .get(MemoryNamespace::Personal, &entry.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        backend
            .table(TABLE_EMBEDDINGS)
            .get(&key)
            .await
            .unwrap()
            .is_none()
    );
    assert!(vectors.lock().unwrap().is_empty());
}

#[tokio::test]
async fn forget_deletes_sot_when_vector_namespace_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = MemoryEngine::new(
        Arc::new(backend.table(TABLE_MEMORIES)),
        Arc::new(backend.table(TABLE_EMBEDDINGS)),
        Box::new(FailOnceRemoveVectorManager {
            vectors: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            fail_next_remove: false,
            namespace_missing_on_remove: true,
        }),
    );

    let entry = engine
        .remember(
            MemoryNamespace::Personal,
            "namespace unavailable forced-delete",
            &[1.0; DIM as usize],
            json!({}),
        )
        .await
        .unwrap();

    engine
        .forget(MemoryNamespace::Personal, &entry.id)
        .await
        .unwrap();
    assert!(
        engine
            .get(MemoryNamespace::Personal, &entry.id)
            .await
            .unwrap()
            .is_none()
    );
}

// ── Fix 回归：chunk i 自身部分写入（memories 已落、embeddings 失败）也被回收 ──

#[tokio::test]
async fn document_index_partial_chunk_write_is_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);

    // 第 2 次 embeddings.set 失败：chunk 0 完整写入，chunk 1 只写了 memories 行
    let embeddings: Arc<dyn KvStore> = Arc::new(FailingKv::new(backend.table(TABLE_EMBEDDINGS), 2));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    use rust_agent::memory::VectorIndexManager as _;
    manager
        .create_index(MemoryNamespace::Document, test_config())
        .await
        .unwrap();
    let engine = MemoryEngine::new(
        Arc::new(backend.table(TABLE_MEMORIES)),
        embeddings,
        Box::new(manager),
    );
    let model: Arc<dyn ModelClient> = Arc::new(MockModel {
        response: String::new(),
    });
    let mut store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        model,
    );

    let content = "# Setup\n\nInstall rustup and the stable toolchain.\n\n# Testing\n\nRun cargo test for the workspace.";
    assert!(store.index_content("guide.md", content).await.is_err());

    // 旧行为：清理只覆盖 0..i，chunk 1 的 memories 行成为孤儿
    let memories = backend.table(TABLE_MEMORIES);
    assert!(memories.list_prefix("document/").await.unwrap().is_empty());
    let embeddings = backend.table(TABLE_EMBEDDINGS);
    assert!(
        embeddings
            .list_prefix("document/")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.list_docs().await.unwrap().is_empty());
}

// ── Fix 回归：chunk 全部写入后 doc/hash meta 写入失败同样回收 ──

#[tokio::test]
async fn document_meta_write_failure_cleans_up_chunks() {
    // fail_on_set=1 → doc/ 行写入失败；=2 → hash/ 行写入失败（doc/ 行需撤销）
    for fail_on_set in [1usize, 2] {
        let dir = tempfile::tempdir().unwrap();
        let backend = open_backend(&dir);
        let engine = build_engine(&backend, MemoryNamespace::Document).await;
        let model: Arc<dyn ModelClient> = Arc::new(MockModel {
            response: String::new(),
        });
        let documents: Arc<dyn KvStore> =
            Arc::new(FailingKv::new(backend.table(TABLE_DOCUMENTS), fail_on_set));
        let mut store = LocalDocumentStore::new(
            documents,
            Arc::new(futures::lock::Mutex::new(engine)),
            model,
        );

        let content = "# Setup\n\nInstall rustup.\n\n# Testing\n\nRun cargo test.";
        assert!(store.index_content("guide.md", content).await.is_err());

        let memories = backend.table(TABLE_MEMORIES);
        assert!(
            memories.list_prefix("document/").await.unwrap().is_empty(),
            "fail_on_set={fail_on_set}: orphan chunks left in memories"
        );
        let embeddings = backend.table(TABLE_EMBEDDINGS);
        assert!(
            embeddings
                .list_prefix("document/")
                .await
                .unwrap()
                .is_empty()
        );
        let raw_documents = backend.table(TABLE_DOCUMENTS);
        assert!(raw_documents.list_prefix("doc/").await.unwrap().is_empty());
        assert!(raw_documents.list_prefix("hash/").await.unwrap().is_empty());
    }
}

// ── Fix 回归：forget 可删除损坏行（JSON 与 Envelope 两级损坏）──

#[tokio::test]
async fn forget_removes_corrupt_rows() {
    let dir = tempfile::tempdir().unwrap();
    {
        // JSON 级损坏：Envelope 合法但内容不是 MemoryEntry
        let backend = open_backend(&dir);
        let memories = backend.table(TABLE_MEMORIES);
        memories
            .set(
                "personal/corrupt-json",
                &json!(["not", "an", "entry"]),
                None,
            )
            .await
            .unwrap();
    }
    {
        // Envelope 级损坏：绕过信封直接写裸字节（redb 独占锁：需先释放句柄）
        use redb::TableDefinition;
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(TABLE_MEMORIES);
        let db = redb::Database::open(dir.path().join("ains.redb")).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(def).unwrap();
            table
                .insert("personal/corrupt-env", [0xFFu8; 3].as_slice())
                .unwrap();
        }
        write.commit().unwrap();
    }

    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;
    // 旧行为：get 解码失败 → forget 永远报 Serialization，损坏行无法删除
    engine
        .forget(MemoryNamespace::Personal, "corrupt-json")
        .await
        .unwrap();
    engine
        .forget(MemoryNamespace::Personal, "corrupt-env")
        .await
        .unwrap();

    let memories = backend.table(TABLE_MEMORIES);
    assert_eq!(memories.get("personal/corrupt-json").await.unwrap(), None);
    assert_eq!(memories.get("personal/corrupt-env").await.unwrap(), None);
}

// ── Fix 回归：索引 add 失败回滚落盘行，不留孤儿占用容量 ──

#[tokio::test]
async fn index_add_failure_rolls_back_sot_rows() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    // 维度不符：ensure_finite 通过，SoT 落盘后 index.add 报错
    let wrong_dim = vec![1.0f32; (DIM + 1) as usize];
    assert!(
        engine
            .remember(
                MemoryNamespace::Personal,
                "bad dim fact",
                &wrong_dim,
                json!({})
            )
            .await
            .is_err()
    );
    assert!(
        engine
            .insert_with_id(
                MemoryNamespace::Personal,
                "chunk-bad",
                "bad dim chunk",
                &wrong_dim,
                json!({})
            )
            .await
            .is_err()
    );

    // 旧行为：memories/embeddings/签名行成为孤儿，永久占用容量
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 0);
    let memories = backend.table(TABLE_MEMORIES);
    assert!(memories.list_prefix("personal/").await.unwrap().is_empty());
    assert!(
        memories
            .list_prefix("sig/personal/")
            .await
            .unwrap()
            .is_empty()
    );
    let embeddings = backend.table(TABLE_EMBEDDINGS);
    assert!(
        embeddings
            .list_prefix("personal/")
            .await
            .unwrap()
            .is_empty()
    );

    // 回滚后引擎仍可正常写入同内容（签名未被孤儿映射占用）
    let entry = engine
        .remember(
            MemoryNamespace::Personal,
            "bad dim fact",
            &embed_text("bad dim fact"),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
    assert!(entry.id.starts_with("mem-"));
}

#[tokio::test]
async fn dedupe_refresh_index_failure_restores_previous_state() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let content = "refresh rollback fact";
    let good = embed_text(content);
    let first = engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &good,
            json!({"importance": 1.0}),
        )
        .await
        .unwrap();

    // 同签名刷新携错误维度向量：索引 add 失败，SoT 应回滚到刷新前
    let wrong_dim = vec![1.0f32; (DIM + 1) as usize];
    assert!(
        engine
            .remember(
                MemoryNamespace::Personal,
                content,
                &wrong_dim,
                json!({"importance": 9.0})
            )
            .await
            .is_err()
    );

    // 条目保持刷新前状态：importance 未被污染，旧向量仍可精确命中
    let entry = engine
        .get(MemoryNamespace::Personal, &first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.metadata["importance"], json!(1.0));
    let hits = engine
        .search(MemoryNamespace::Personal, &good, 1)
        .await
        .unwrap();
    assert_eq!(hits[0].0.id, first.id);
    assert!(hits[0].1 > 0.999);

    // 重建后（embeddings 已回滚）同样精确命中
    drop(engine);
    let engine2 = build_engine(&backend, MemoryNamespace::Personal).await;
    let hits = engine2
        .search(MemoryNamespace::Personal, &good, 1)
        .await
        .unwrap();
    assert!(hits[0].1 > 0.999);
}

#[tokio::test]
async fn dedupe_refresh_failure_restores_resident_index() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let vectors = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let manager = MutatingThenFailingVectorManager {
        vectors: Arc::clone(&vectors),
        adds: 0,
        fail_on_add: 2,
    };
    let mut engine = MemoryEngine::new(
        Arc::new(backend.table(TABLE_MEMORIES)),
        Arc::new(backend.table(TABLE_EMBEDDINGS)),
        Box::new(manager),
    );

    let content = "resident-index rollback fact";
    let original = vec![1.0; DIM as usize];
    let refreshed = vec![0.0; DIM as usize];
    let first = engine
        .remember(MemoryNamespace::Personal, content, &original, json!({}))
        .await
        .unwrap();

    // The manager stores `refreshed` before returning an error.  The engine
    // must replay the old embedding into the resident index during rollback.
    assert!(
        engine
            .remember(MemoryNamespace::Personal, content, &refreshed, json!({}))
            .await
            .is_err()
    );

    assert_eq!(
        vectors
            .lock()
            .unwrap()
            .get(&MemoryNamespace::Personal.storage_key(&first.id))
            .cloned(),
        Some(original)
    );
}

#[tokio::test]
async fn failed_new_remember_removes_resident_index_node() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let vectors = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let manager = MutatingThenFailingVectorManager {
        vectors: Arc::clone(&vectors),
        adds: 0,
        fail_on_add: 1,
    };
    let mut engine = MemoryEngine::new(
        Arc::new(backend.table(TABLE_MEMORIES)),
        Arc::new(backend.table(TABLE_EMBEDDINGS)),
        Box::new(manager),
    );

    assert!(
        engine
            .remember(
                MemoryNamespace::Personal,
                "new index rollback fact",
                &[1.0; DIM as usize],
                json!({}),
            )
            .await
            .is_err()
    );
    assert!(vectors.lock().unwrap().is_empty());
}

// ── 7.4 冷启动优化：向量索引懒加载（首次检索才重建） ──

#[tokio::test]
async fn create_index_is_lazy_and_first_search_rebuilds_from_sot() {
    use rust_agent::memory::{VectorIndexManager, vector_to_value};

    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_EMBEDDINGS));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let ns = MemoryNamespace::Personal;

    // 模拟上次会话遗留的 Source Of Truth：直接把向量写入 embeddings 表。
    let v_apple = embed_text("apple");
    embeddings
        .set(&ns.storage_key("n-apple"), &vector_to_value(&v_apple), None)
        .await
        .unwrap();

    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    manager.create_index(ns, test_config()).await.unwrap();

    // 冷启动核心：create_index 只登记配置，不触发整表加载 / 图重建。
    assert!(
        !manager.is_loaded(ns),
        "create_index 应懒加载，不应立即物化索引"
    );

    // 首次检索才从 SoT 懒加载重建，并正确命中遗留向量。
    let hits = manager.search(ns, &v_apple, 5).await.unwrap();
    assert!(manager.is_loaded(ns), "首次 search 后索引应已物化");
    assert_eq!(hits[0].0, "n-apple");
    assert!(hits[0].1 > 0.999);

    // 未被检索的其他 namespace 永不重建（冷启动只为用到的 namespace 付费）。
    manager
        .create_index(MemoryNamespace::Document, test_config())
        .await
        .unwrap();
    assert!(!manager.is_loaded(MemoryNamespace::Document));
}

#[tokio::test]
async fn pending_index_absorbs_writes_without_rebuild() {
    use rust_agent::memory::{VectorIndexManager, vector_to_value};

    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_EMBEDDINGS));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let ns = MemoryNamespace::Personal;

    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    manager.create_index(ns, test_config()).await.unwrap();

    // SoT 先行（模拟引擎写入契约）：先落盘 embeddings，再调用 index.add。
    let v = embed_text("banana");
    embeddings
        .set(&ns.storage_key("n-banana"), &vector_to_value(&v), None)
        .await
        .unwrap();
    manager.add(ns, "n-banana", &v).await.unwrap();
    // 写入不应物化未加载索引：只有检索才重建（首次检索才重建）。
    assert!(!manager.is_loaded(ns), "add 不应物化未加载索引");

    // 被 no-op add 的条目经 SoT 懒加载重建后仍可命中。
    let hits = manager.search(ns, &v, 5).await.unwrap();
    assert!(manager.is_loaded(ns));
    assert_eq!(hits[0].0, "n-banana");

    // 维度不符即使索引未物化也须在写时报错（供上层及时回滚 SoT）。
    let ns2 = MemoryNamespace::Document;
    manager.create_index(ns2, test_config()).await.unwrap();
    let wrong = vec![1.0f32; (DIM + 1) as usize];
    assert!(matches!(
        manager.add(ns2, "bad", &wrong).await,
        Err(MemoryError::Storage(_))
    ));
    assert!(!manager.is_loaded(ns2), "写时维度校验失败不应物化索引");
}

// ── 7.5 隐私审计：本地数据静态加密（EncryptedKvStore 包真实 redb 表） ──

#[tokio::test]
async fn encrypted_kv_store_roundtrip_and_ciphertext_at_rest() {
    use rust_agent::memory::{EncryptedKvStore, EncryptionKey};

    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    // 同一张底层表：`raw` 直接读写（观测静态形态），`enc` 经加密装饰器。
    let raw: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let enc = EncryptedKvStore::new(Arc::clone(&raw), EncryptionKey::from_bytes([42u8; 32]));

    let secret = json!({"token": "SECRET-VALUE-XYZ", "n": 7});
    enc.set("kv/session", &secret, None).await.unwrap();

    // 经装饰器读回 = 原值。
    assert_eq!(enc.get("kv/session").await.unwrap(), Some(secret.clone()));

    // 直读底层：存的是密文信封，且序列化后不含明文。
    let at_rest = raw.get("kv/session").await.unwrap().unwrap();
    assert!(at_rest.get("__ains_sealed_v").is_some(), "底层应存密文信封");
    let at_rest_str = serde_json::to_string(&at_rest).unwrap();
    assert!(!at_rest_str.contains("SECRET-VALUE-XYZ"), "底层不得含明文");

    // key 明文：前缀列举不受加密影响。
    assert_eq!(
        enc.list_prefix("kv/").await.unwrap(),
        vec!["kv/session".to_string()]
    );

    // 错误密钥无法解密（而非静默返回错误明文）。
    let wrong = EncryptedKvStore::new(Arc::clone(&raw), EncryptionKey::from_bytes([1u8; 32]));
    assert!(wrong.get("kv/session").await.is_err());

    // delete 透传生效。
    enc.delete("kv/session").await.unwrap();
    assert_eq!(enc.get("kv/session").await.unwrap(), None);
}

#[tokio::test]
async fn encrypted_kv_store_forwards_ttl() {
    use rust_agent::memory::{EncryptedKvStore, EncryptionKey};

    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let raw: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let enc = EncryptedKvStore::new(raw, EncryptionKey::from_bytes([5u8; 32]));

    // TTL 存于底层明文信封，过期惰性回收不需解密。
    enc.set(
        "kv/short",
        &json!({"x": 1}),
        Some(Duration::from_millis(10)),
    )
    .await
    .unwrap();
    enc.set("kv/keep", &json!({"y": 2}), Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(enc.get("kv/short").await.unwrap(), None);
    assert_eq!(enc.get("kv/keep").await.unwrap(), Some(json!({"y": 2})));
}

// ── 7+.3 子代理 Swarm：KV 信箱 IPC（包真实 redb 表） ──

#[tokio::test]
async fn kv_mailbox_post_inbox_unread_mark_read() {
    use rust_agent::swarm::KvMailbox;

    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let kv: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let mbox = KvMailbox::new(kv);

    // lead 向 researcher 投递两条；tester 一条（隔离）
    let m1 = mbox
        .post("lead", "researcher", "gather sources")
        .await
        .unwrap();
    let _m2 = mbox
        .post("lead", "researcher", "then summarize")
        .await
        .unwrap();
    mbox.post("lead", "tester", "run suite").await.unwrap();

    // researcher 收件箱有 2 条，按投递时间升序
    let inbox = mbox.inbox("researcher").await.unwrap();
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox[0].body, "gather sources");
    assert_eq!(inbox[1].body, "then summarize");
    assert_eq!(inbox[0].sender, "lead");
    // namespace 隔离：tester 不受影响
    assert_eq!(mbox.inbox("tester").await.unwrap().len(), 1);

    // 全部未读 → 标记 m1 已读 → 剩 1 未读
    assert_eq!(mbox.unread("researcher").await.unwrap().len(), 2);
    mbox.mark_read("researcher", &m1.id).await.unwrap();
    let unread = mbox.unread("researcher").await.unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].body, "then summarize");

    // 不存在的消息报 NotFound
    assert!(matches!(
        mbox.mark_read("researcher", "0000000000000-deadbeef").await,
        Err(MemoryError::NotFound(_))
    ));
}

// ── Fix 回归：remember 写入中途 KvStore 落盘失败回滚，不留孤儿 / 不破坏去重 ──

#[tokio::test]
async fn remember_rolls_back_on_embeddings_set_failure() {
    // embeddings 第 1 次 set 失败 = remember 的向量落盘步；旧行为：`?` 直接
    // 上抛，已写入的 memories 条目行成为孤儿，永久占用 count 统计的容量。
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let embeddings: Arc<dyn KvStore> = Arc::new(FailingKv::new(backend.table(TABLE_EMBEDDINGS), 1));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    use rust_agent::memory::VectorIndexManager as _;
    manager
        .create_index(MemoryNamespace::Personal, test_config())
        .await
        .unwrap();
    let mut engine = MemoryEngine::new(
        Arc::new(backend.table(TABLE_MEMORIES)),
        embeddings,
        Box::new(manager),
    );

    let content = "rollback on embeddings failure";
    assert!(
        engine
            .remember(
                MemoryNamespace::Personal,
                content,
                &embed_text(content),
                json!({})
            )
            .await
            .is_err()
    );

    // 条目行、签名行、向量行都不应残留；容量不泄漏
    let memories = backend.table(TABLE_MEMORIES);
    assert!(memories.list_prefix("personal/").await.unwrap().is_empty());
    assert!(
        memories
            .list_prefix("sig/personal/")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        backend
            .table(TABLE_EMBEDDINGS)
            .list_prefix("personal/")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 0);
}

#[tokio::test]
async fn remember_rolls_back_on_signature_set_failure() {
    // memories 第 2 次 set 失败 = remember 的签名落盘步（第 1 次是条目行）；
    // 旧行为：条目 + 向量已落盘但无签名 → 下次同内容 remember 无法去重、重复建条目。
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_EMBEDDINGS));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    use rust_agent::memory::VectorIndexManager as _;
    manager
        .create_index(MemoryNamespace::Personal, test_config())
        .await
        .unwrap();
    let memories: Arc<dyn KvStore> = Arc::new(FailingKv::new(backend.table(TABLE_MEMORIES), 2));
    let mut engine = MemoryEngine::new(memories, embeddings, Box::new(manager));

    let content = "rollback on signature failure";
    assert!(
        engine
            .remember(
                MemoryNamespace::Personal,
                content,
                &embed_text(content),
                json!({})
            )
            .await
            .is_err()
    );

    // 条目行 + 向量行都应被回滚，无“有内容无签名”孤儿
    let raw_memories = backend.table(TABLE_MEMORIES);
    assert!(
        raw_memories
            .list_prefix("personal/")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        raw_memories
            .list_prefix("sig/personal/")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        backend
            .table(TABLE_EMBEDDINGS)
            .list_prefix("personal/")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 0);
}

// ── Fix 回归：memdir 超大 ttl_days 饱和而非溢出 ──

#[tokio::test]
async fn memdir_huge_ttl_days_saturates() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    // ttl_days 来自模型输出（不可信）；旧行为：乘法溢出
    // debug 下 panic，release 下回绕成“已过期”被 scan 过滤
    store
        .add_entry(NewMemoryEntry {
            title: "Long Lived".into(),
            body: "entry with absurd ttl from model output".into(),
            ttl_days: i64::MAX,
            ..Default::default()
        })
        .await
        .unwrap();

    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "Long Lived");
}

// ── Fix 回归：search 回填跳过损坏 memories 行，不毒化整次检索 ──

#[tokio::test]
async fn search_skips_corrupt_memory_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut ids = Vec::new();
    {
        let backend = open_backend(&dir);
        let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;
        for text in ["alpha fact", "beta note", "gamma idea"] {
            let entry = engine
                .remember(
                    MemoryNamespace::Personal,
                    text,
                    &embed_text(text),
                    json!({}),
                )
                .await
                .unwrap();
            ids.push(entry.id);
        }
        // JSON 级损坏：Envelope 合法但内容不是 MemoryEntry
        let memories = backend.table(TABLE_MEMORIES);
        memories
            .set(
                &format!("personal/{}", ids[0]),
                &json!(["not", "an", "entry"]),
                None,
            )
            .await
            .unwrap();
    }
    {
        // Envelope 级损坏：绕过信封直接写裸字节（redb 独占锁：需先释放句柄）
        use redb::TableDefinition;
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(TABLE_MEMORIES);
        let db = redb::Database::open(dir.path().join("ains.redb")).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(def).unwrap();
            table
                .insert(
                    format!("personal/{}", ids[1]).as_str(),
                    [0xFFu8; 3].as_slice(),
                )
                .unwrap();
        }
        write.commit().unwrap();
    }

    // 旧行为：任一损坏行被命中即整次 search 报 Serialization 错误
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Personal).await;
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text("gamma idea"), 3)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.content, "gamma idea");
}

// ── Fix 回归：惰性过期删除写事务内复核（不误删并发刷新的新值）──
//
// 真实竞态交置（get 判定过期后、删除前插入 set）无法在公开 API 上
// 确定性复现；本用例仅守卫“过期行刷新后新值存活”的不变量，
// 竞态闭合本身由 remove_if_expired 的写事务内复核保证。

#[tokio::test]
async fn expired_read_then_refresh_keeps_new_value() {
    let dir = tempfile::tempdir().unwrap();
    let kv = open_backend(&dir).table(TABLE_KV);

    // 先写一条已过期数据，触发一次惰性删除路径
    kv.set("race/key", &json!("stale"), Some(Duration::from_millis(1)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(kv.get("race/key").await.unwrap(), None);

    // 刷新后新值必须存活，get / list_prefix / sweep 均不得误删
    kv.set("race/key", &json!("fresh"), None).await.unwrap();
    assert_eq!(kv.get("race/key").await.unwrap(), Some(json!("fresh")));
    assert_eq!(kv.sweep_expired().await.unwrap(), 0);
    assert_eq!(kv.list_prefix("race/").await.unwrap(), vec!["race/key"]);
}

// ── Fix 回归：损坏行生命周期闭环（去重回落 / 淘汰优先回收）──

#[tokio::test]
async fn remember_falls_back_when_dedupe_target_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let content = "corrupt dedupe fact";
    let first = engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();

    // 目标行 JSON 级损坏：签名仍指向它，旧行为 remember 永久报错
    let memories = backend.table(TABLE_MEMORIES);
    memories
        .set(
            &format!("personal/{}", first.id),
            &json!(["not", "an", "entry"]),
            None,
        )
        .await
        .unwrap();

    let second = engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();
    assert_ne!(second.id, first.id);

    // 检索：损坏旧行被跳过，新条目可命中
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text(content), 2)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.id, second.id);
}

#[tokio::test]
async fn capacity_evicts_current_domain_before_unknown_corrupt_row() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal)
        .await
        .with_max_entries(2);

    engine
        .remember(
            MemoryNamespace::Personal,
            "good one",
            &embed_text("good one"),
            json!({"importance": 0.1}),
        )
        .await
        .unwrap();
    // 损坏行占满容量。它没有 dedupe_domain，因而无法安全判定所属 owner /
    // project；容量回收不得为腾位而删除它，否则当前账户可删掉另一账户的
    // 损坏但仍可恢复数据。
    let memories = backend.table(TABLE_MEMORIES);
    memories
        .set("personal/zzz-corrupt", &json!("just a string"), None)
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 2);

    engine
        .remember(
            MemoryNamespace::Personal,
            "good two",
            &embed_text("good two"),
            json!({}),
        )
        .await
        .unwrap();

    // 未知归属的损坏行不得删除；当前 domain 中的低分有效条目可安全淘汰。
    assert!(
        memories
            .get("personal/zzz-corrupt")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 2);
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text("good one"), 2)
        .await
        .unwrap();
    let contents: Vec<&str> = hits.iter().map(|(e, _)| e.content.as_str()).collect();
    assert!(!contents.contains(&"good one"), "hits: {hits:?}");
    assert!(contents.contains(&"good two"), "hits: {hits:?}");
}

// ── Fix 回归：memdir scan 不被损坏行毒化 ──

#[tokio::test]
async fn memdir_scan_survives_corrupt_entry_row() {
    let dir = tempfile::tempdir().unwrap();
    {
        let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
        let store = MemdirStore::new(kv);
        store
            .add_entry(NewMemoryEntry {
                title: "Valid Entry".into(),
                body: "survives corrupt sibling row".into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    {
        // 手工构造 Envelope 合法但 JSON 载荷非法的行：
        // bincode(Envelope{expires_at_ms: None, json: "not jso"})
        // = [0u8] + u64 长度 LE + 载荷字节（list_prefix 能列出，get 报
        // Serialization，旧行为毒化 scan_raw）
        let payload = b"not jso";
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(payload);
        use redb::TableDefinition;
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(TABLE_KV);
        let db = redb::Database::open(dir.path().join("ains.redb")).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(def).unwrap();
            table
                .insert("memdir/entries/broken.md", bytes.as_slice())
                .unwrap();
        }
        write.commit().unwrap();
    }

    let kv: Arc<dyn KvStore> = Arc::new(
        RedbBackend::open(dir.path().join("ains.redb"))
            .unwrap()
            .table(TABLE_KV),
    );
    let store = MemdirStore::new(kv);
    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "Valid Entry");
    // 去重扫描同样经过 scan_raw：add 仍可用
    store
        .add_entry(NewMemoryEntry {
            title: "Another".into(),
            body: "add still works with corrupt row present".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(store.scan(10).await.unwrap().len(), 2);
}

// ── Fix 回归：损坏行文件名被复用自愈（M2）──

#[tokio::test]
async fn memdir_reuses_corrupt_row_filename_self_heals() {
    let dir = tempfile::tempdir().unwrap();
    // 先写入一条合法条目，产生 `build.md` 文件名。
    {
        let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
        let store = MemdirStore::new(kv);
        store
            .add_entry(NewMemoryEntry {
                title: "Build".into(),
                body: "original build body".into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // 把 `build.md` 的行替换为 Envelope 合法但 JSON 载荷非法的损坏行，
    // 模拟条目被写坏（`list_prefix` 能列出该键，`scan_raw`/`get` 报
    // Serialization）。
    {
        let payload = b"not jso";
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(payload);
        use redb::TableDefinition;
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(TABLE_KV);
        let db = redb::Database::open(dir.path().join("ains.redb")).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(def).unwrap();
            table
                .insert("memdir/entries/build.md", bytes.as_slice())
                .unwrap();
        }
        write.commit().unwrap();
    }
    // 同名但不同正文：不触发签名去重，`unique_filename` 应识别该键为损坏
    // 行并复用 `build.md`（而非退避成 `build_2.md`），随后覆写完成自愈。
    {
        let kv: Arc<dyn KvStore> = Arc::new(
            RedbBackend::open(dir.path().join("ains.redb"))
                .unwrap()
                .table(TABLE_KV),
        );
        let store = MemdirStore::new(kv);
        let filename = store
            .add_entry(NewMemoryEntry {
                title: "Build".into(),
                body: "rebuilt build body".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(filename, "build.md", "corrupt row filename must be reused");
        let entries = store.scan(10).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "corrupt row must be healed, not duplicated"
        );
        assert_eq!(entries[0].body.trim(), "rebuilt build body");
    }
}

// ── Fix 回归：upsert 索引行不产生多余空行且清洗标题（L2）──

#[tokio::test]
async fn memdir_upsert_index_line_no_double_newline_and_sanitizes_title() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    // 初次写入产生单行索引：`- [Alpha](alpha.md)\n`。
    store
        .add_entry(NewMemoryEntry {
            title: "Alpha".into(),
            body: "same body".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    // 同签名（同 body）再写 → 去重刷新走 `upsert_index_line`，标题含
    // `[`/`]` 应被清洗，且重写后不能出现多余空行。
    store
        .add_entry(NewMemoryEntry {
            title: "Be]ta[".into(),
            body: "same body".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let index = store.read_index().await.unwrap().unwrap();
    assert_eq!(
        index, "- [Beta](alpha.md)\n",
        "title sanitized and no double trailing newline; got: {index:?}"
    );
    assert!(!index.contains("\n\n"), "no blank lines allowed: {index:?}");
}

// ── Fix 回归：文档 meta 损坏不阻断 list / 重新索引 ──

#[tokio::test]
async fn document_dedupe_and_list_survive_corrupt_meta() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Document).await;
    let model: Arc<dyn ModelClient> = Arc::new(MockModel {
        response: String::new(),
    });
    let mut store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        Arc::clone(&model),
    );

    let content = "# Setup\n\nInstall rustup.\n\n# Testing\n\nRun cargo test.";
    let meta = store.index_content("guide.md", content).await.unwrap();

    // meta 行 JSON 级损坏：旧行为 list_docs 报错、同内容重新索引报错
    let documents = backend.table(TABLE_DOCUMENTS);
    documents
        .set(&format!("doc/{}", meta.id), &json!(["bad"]), None)
        .await
        .unwrap();

    assert!(store.list_docs().await.unwrap().is_empty());
    // hash 行仍在：is_indexed 不受 meta 损坏影响
    assert!(store.is_indexed(&meta.source_hash).await.unwrap());

    // 同内容重新索引：meta 损坏视为未命中，覆写修复
    let again = store.index_content("guide.md", content).await.unwrap();
    assert_eq!(again.source_hash, meta.source_hash);
    assert!(again.chunk_count >= 1);
    let docs = store.list_docs().await.unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].name, "guide.md");
}

// ── Fix 回归：HNSW 同 id 更新不受活跃容量限制 ──

#[tokio::test]
async fn hnsw_update_at_capacity_succeeds() {
    use rust_agent::memory::{HnswVectorIndex, VectorIndex};

    let mut index =
        HnswVectorIndex::new(MemoryNamespace::Personal, test_config()).with_max_entries(2);
    index.add("a", &embed_text("alpha")).await.unwrap();
    index.add("b", &embed_text("beta")).await.unwrap();

    // 容量已满：同 id 更新（活跃数不变）必须成功。旧行为：
    // capacity exceeded → 去重刷新在容量满时永久失败并触发回滚
    index.add("a", &embed_text("alpha updated")).await.unwrap();
    let hits = index.search(&embed_text("alpha updated"), 1).await.unwrap();
    assert_eq!(hits[0].0, "a");
    assert!(hits[0].1 > 0.999);

    // 新 id 仍受活跃容量限制
    assert!(index.add("c", &embed_text("gamma")).await.is_err());
}

// ── Fix 回归：去重刷新在旧 embedding 快照行损坏时仍可完成 ──

#[tokio::test]
async fn dedupe_refresh_survives_corrupt_previous_embedding() {
    let dir = tempfile::tempdir().unwrap();
    let content = "refresh with corrupt snapshot";
    let first_id: String;
    {
        let backend = open_backend(&dir);
        let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;
        first_id = engine
            .remember(
                MemoryNamespace::Personal,
                content,
                &embed_text(content),
                json!({}),
            )
            .await
            .unwrap()
            .id;
    }
    {
        // Envelope 级损坏旧 embedding 行（redb 独占锁：需先释放句柄）
        use redb::TableDefinition;
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(TABLE_EMBEDDINGS);
        let db = redb::Database::open(dir.path().join("ains.redb")).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(def).unwrap();
            table
                .insert(
                    format!("personal/{first_id}").as_str(),
                    [0xFFu8; 3].as_slice(),
                )
                .unwrap();
        }
        write.commit().unwrap();
    }

    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;
    // 旧行为：读取回滚快照报 Serialization → 同签名刷新永久失败
    let refreshed = engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(refreshed.id, first_id);

    // 损坏行已被新 embedding 覆写：可精确检索，重建后仍有效
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text(content), 1)
        .await
        .unwrap();
    assert_eq!(hits[0].0.id, first_id);
    assert!(hits[0].1 > 0.999);
    drop(engine);
    let engine2 = build_engine(&backend, MemoryNamespace::Personal).await;
    let hits = engine2
        .search(MemoryNamespace::Personal, &embed_text(content), 1)
        .await
        .unwrap();
    assert!(hits[0].1 > 0.999);
}

// ── Fix 回归：doc meta 损坏不阻断 delete（chunk / hash 行全量回收）──

#[tokio::test]
async fn document_delete_survives_corrupt_meta() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Document).await;
    let model: Arc<dyn ModelClient> = Arc::new(MockModel {
        response: String::new(),
    });
    let mut store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        Arc::clone(&model),
    );

    let content = "# Setup\n\nInstall rustup.\n\n# Testing\n\nRun cargo test.";
    let meta = store.index_content("guide.md", content).await.unwrap();

    // meta 行 JSON 级损坏：旧行为 delete 报 Serialization，chunk/hash 永久残留
    let documents = backend.table(TABLE_DOCUMENTS);
    documents
        .set(&format!("doc/{}", meta.id), &json!(["bad"]), None)
        .await
        .unwrap();

    store.delete(&meta.id).await.unwrap();

    let memories = backend.table(TABLE_MEMORIES);
    assert!(memories.list_prefix("document/").await.unwrap().is_empty());
    let embeddings = backend.table(TABLE_EMBEDDINGS);
    assert!(
        embeddings
            .list_prefix("document/")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(documents.list_prefix("doc/").await.unwrap().is_empty());
    assert!(documents.list_prefix("hash/").await.unwrap().is_empty());
    assert!(!store.is_indexed(&meta.source_hash).await.unwrap());
}

// ── Fix 回归：Euclidean 分数口径（Native -DistL2 与双端共享函数一致）──

#[tokio::test]
async fn euclidean_scores_match_shared_similarity_fn() {
    use rust_agent::memory::{VectorIndexManager as _, similarity_score};

    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_EMBEDDINGS));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    manager
        .create_index(
            MemoryNamespace::Personal,
            VectorIndexConfig {
                dimension: DIM,
                distance_metric: Metric::Euclidean,
                m: 16,
                ef: 50,
            },
        )
        .await
        .unwrap();
    let mut engine = MemoryEngine::new(
        Arc::new(backend.table(TABLE_MEMORIES)),
        embeddings,
        Box::new(manager),
    );

    for text in ["alpha fact", "coffee brewing"] {
        engine
            .remember(
                MemoryNamespace::Personal,
                text,
                &embed_text(text),
                json!({}),
            )
            .await
            .unwrap();
    }

    let query = embed_text("alpha fact");
    let hits = engine
        .search(MemoryNamespace::Personal, &query, 2)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.content, "alpha fact");
    // 精确命中：距离 0 → 分数 0；排序“越大越相近”
    assert!(hits[0].1.abs() < 1e-4);
    assert!(hits[0].1 > hits[1].1);
    // Native 分数（-DistL2，含 sqrt）必须与双端共享的 similarity_score
    // 口径一致，否则 Native/Web 检索分数不可比较
    for (entry, score) in &hits {
        let expected = similarity_score(Metric::Euclidean, &query, &embed_text(&entry.content));
        assert!(
            (score - expected).abs() < 1e-4,
            "native score {score} != shared fn {expected} for {:?}",
            entry.content
        );
    }
}

// ── Fix 回归：clear_namespace 幂等（remove_index ensure-absent 语义）──

#[tokio::test]
async fn clear_namespace_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    let content = "idempotent clear";
    engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();

    engine
        .clear_namespace(MemoryNamespace::Personal)
        .await
        .unwrap();
    // 旧行为：第二次 remove_index 报 NamespaceNotFound
    engine
        .clear_namespace(MemoryNamespace::Personal)
        .await
        .unwrap();

    // 重建索引后可继续写入
    engine
        .vector
        .create_index(MemoryNamespace::Personal, test_config())
        .await
        .unwrap();
    engine
        .remember(
            MemoryNamespace::Personal,
            content,
            &embed_text(content),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
}

// ── Review 二轮回归：memdir TTL 以 updated_at 为锚（基线 schema.py），刷新延长寿命 ──

#[tokio::test]
async fn memdir_ttl_anchors_to_updated_at_and_refresh_revives() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(Arc::clone(&kv));

    let body = "Use blue-green deploys for the API.";
    let filename = store
        .add_entry(NewMemoryEntry {
            title: "Deploy Notes".into(),
            body: body.into(),
            ttl_days: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    let key = format!("memdir/entries/{filename}");

    let ten_days_ago = format_iso_utc(now_ms() - 10 * 24 * 3600 * 1000);
    let fresh = format_iso_utc(now_ms());

    // created_at 超出 TTL、updated_at 新鲜：旧行为（created_at 锚）误判
    // 过期，基线锚定 updated_at 应仍可见
    let raw = kv.get(&key).await.unwrap().unwrap();
    let text = rewrite_entry_timestamps(raw.as_str().unwrap(), &ten_days_ago, &fresh);
    kv.set(&key, &serde_json::Value::String(text), None)
        .await
        .unwrap();
    assert_eq!(store.scan(10).await.unwrap().len(), 1);

    // 双时间戳都超出 TTL → 不可见
    let raw = kv.get(&key).await.unwrap().unwrap();
    let text = rewrite_entry_timestamps(raw.as_str().unwrap(), &ten_days_ago, &ten_days_ago);
    kv.set(&key, &serde_json::Value::String(text), None)
        .await
        .unwrap();
    assert!(store.scan(10).await.unwrap().is_empty());

    // 同签名 add_entry 刷新 updated_at → 复活。旧行为：刷新只写
    // updated_at 而 scan 按 created_at 过滤，写入报成功却永久不可见
    let dup = store
        .add_entry(NewMemoryEntry {
            title: "Deploy Notes".into(),
            body: body.into(),
            ttl_days: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(dup, filename);
    assert_eq!(store.scan(10).await.unwrap().len(), 1);
}

// ── Review 二轮回归：memdir 新建路径 importance 钳位 ≥ 1（与刷新路径/基线同口径）──

#[tokio::test]
async fn memdir_new_entry_importance_clamped_to_min_one() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(kv);

    store
        .add_entry(NewMemoryEntry {
            title: "Low Importance".into(),
            body: "Rate limits are per-tenant.".into(),
            importance: 0.0,
            ..Default::default()
        })
        .await
        .unwrap();

    let entries = store.scan(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    // 旧行为：新建存 0.0，与刷新路径的 .max(1.0) 不一致
    assert_eq!(entries[0].importance, 1.0);
}

// ── Review 二轮回归：容量满淘汰后新条目写入失败，恢复被淘汰条目（无净丢失）──

#[tokio::test]
async fn eviction_is_restored_when_new_entry_write_fails() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    // memories 第 3 次 set 失败 = 新条目的 memories 落盘步
    //（第 1/2 次为首条记忆的条目行与签名行）
    let memories: Arc<dyn KvStore> = Arc::new(FailingKv::new(backend.table(TABLE_MEMORIES), 3));
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_EMBEDDINGS));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    use rust_agent::memory::VectorIndexManager as _;
    manager
        .create_index(MemoryNamespace::Personal, test_config())
        .await
        .unwrap();
    let mut engine = MemoryEngine::new(memories, embeddings, Box::new(manager)).with_max_entries(1);

    let first = engine
        .remember(
            MemoryNamespace::Personal,
            "alpha fact",
            &embed_text("alpha fact"),
            json!({}),
        )
        .await
        .unwrap();
    assert!(
        engine
            .remember(
                MemoryNamespace::Personal,
                "beta idea",
                &embed_text("beta idea"),
                json!({}),
            )
            .await
            .is_err()
    );

    // 旧行为：先淘汰后写入且不恢复 → alpha 被删、beta 未写入，净丢失一条
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
    assert!(
        engine
            .get(MemoryNamespace::Personal, &first.id)
            .await
            .unwrap()
            .is_some()
    );
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text("alpha fact"), 1)
        .await
        .unwrap();
    assert_eq!(hits[0].0.id, first.id);

    // 签名映射同样恢复：同内容 remember 走去重刷新而非重复建条目
    let refreshed = engine
        .remember(
            MemoryNamespace::Personal,
            "alpha fact",
            &embed_text("alpha fact"),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(refreshed.id, first.id);

    // 故障解除后正常淘汰路径仍可用
    engine
        .remember(
            MemoryNamespace::Personal,
            "beta idea",
            &embed_text("beta idea"),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
}

// ── Review 二轮回归：doc_ids 过滤检索在固定过采样不足时扩窗，不欠采样 ──

#[tokio::test]
async fn document_search_doc_filter_widens_oversampling() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Document).await;
    let model: Arc<dyn ModelClient> = Arc::new(MockModel {
        response: String::new(),
    });
    let mut store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        model,
    );

    // 大文档：6 个与查询同向的 chunk（每段超过分块预算一半，
    // 段落无法打包，各自成块）
    let paragraph = "alpha beta gamma delta ".repeat(80);
    let big_content = vec![paragraph; 6].join("\n\n");
    let big = store.index_content("big.txt", &big_content).await.unwrap();
    assert!(
        big.chunk_count > 4,
        "need >4 chunks to exceed the fixed 4x oversampling, got {}",
        big.chunk_count
    );

    // 小文档：单 chunk，与查询相似度低于大文档全部 chunk
    let small = store
        .index_content("small.txt", "zzzz qqqq jjjj xxxx")
        .await
        .unwrap();

    // top_k=1 + 限定小文档：旧行为固定 4 倍过采样只取到大文档 chunk，
    // 过滤后返回空；扩窗后应命中小文档的唯一 chunk
    let results = store
        .search(
            "alpha beta gamma delta",
            1,
            Some(std::slice::from_ref(&small.id)),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].chunk.doc_id, small.id);
}

// ── Review 三轮回归：memdir TTL 过期边界 >=（恰好到期即过期，同基线）──

#[tokio::test]
async fn memdir_ttl_expires_exactly_at_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(Arc::clone(&kv));

    let filename = store
        .add_entry(NewMemoryEntry {
            title: "Boundary Note".into(),
            body: "Cache warmup takes ten minutes.".into(),
            ttl_days: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    let key = format!("memdir/entries/{filename}");
    const DAY_MS: i64 = 24 * 3600 * 1000;

    // 锚点恰好在 1 天前：基线 `now >= base + ttl` → 已过期
    //（秒级截断只会使 base 更早，判定方向不变）
    let at_boundary = format_iso_utc(now_ms() - DAY_MS);
    let raw = kv.get(&key).await.unwrap().unwrap();
    let text = rewrite_entry_timestamps(raw.as_str().unwrap(), &at_boundary, &at_boundary);
    kv.set(&key, &serde_json::Value::String(text), None)
        .await
        .unwrap();
    assert!(store.scan(10).await.unwrap().is_empty());

    // 边界前 1 分钟：未过期
    let before_boundary = format_iso_utc(now_ms() - DAY_MS + 60_000);
    let raw = kv.get(&key).await.unwrap().unwrap();
    let text = rewrite_entry_timestamps(raw.as_str().unwrap(), &before_boundary, &before_boundary);
    kv.set(&key, &serde_json::Value::String(text), None)
        .await
        .unwrap();
    assert_eq!(store.scan(10).await.unwrap().len(), 1);
}

// ── Review 三轮回归：恢复路径自身故障不破坏引擎可用性（best-effort 语义）──

#[tokio::test]
async fn restore_failure_keeps_engine_usable() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    // memories 第 3 次 set = 新条目落盘失败，第 4 次 = 恢复被淘汰条目
    // 的写回也失败（双故障）
    let memories: Arc<dyn KvStore> = Arc::new(FailingKv::failing_at(
        backend.table(TABLE_MEMORIES),
        vec![3, 4],
    ));
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_EMBEDDINGS));
    let hnsw_cache: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_HNSW_CACHE));
    let mut manager = DefaultVectorIndexManager::new(Arc::clone(&embeddings), hnsw_cache);
    use rust_agent::memory::VectorIndexManager as _;
    manager
        .create_index(MemoryNamespace::Personal, test_config())
        .await
        .unwrap();
    let mut engine = MemoryEngine::new(memories, embeddings, Box::new(manager)).with_max_entries(1);

    engine
        .remember(
            MemoryNamespace::Personal,
            "alpha fact",
            &embed_text("alpha fact"),
            json!({}),
        )
        .await
        .unwrap();
    // 原始错误正常上抛（不被恢复失败遮蔽、不 panic）
    assert!(
        engine
            .remember(
                MemoryNamespace::Personal,
                "beta idea",
                &embed_text("beta idea"),
                json!({}),
            )
            .await
            .is_err()
    );
    // best-effort 恢复也失败：alpha 丢失（已记日志），但无孤儿行残留
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 0);

    // 引擎保持可用：故障解除后写入/检索正常
    engine
        .remember(
            MemoryNamespace::Personal,
            "gamma note",
            &embed_text("gamma note"),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(engine.count(MemoryNamespace::Personal).await.unwrap(), 1);
    let hits = engine
        .search(MemoryNamespace::Personal, &embed_text("gamma note"), 1)
        .await
        .unwrap();
    assert_eq!(hits[0].0.content, "gamma note");
}

// ── Review 三轮回归：空 doc_ids 过滤器返回空，不陷入扩窗循环 ──

#[tokio::test]
async fn document_search_empty_doc_filter_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Document).await;
    // 搜索若错误地继续执行 embed，会立即失败；空过滤器必须在此之前返回。
    let model: Arc<dyn ModelClient> = Arc::new(FlakyEmbedModel {
        fail_after: 0,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        model,
    );

    // 空过滤器：无需调用 embedding 服务或查询索引。
    let results = store
        .search("install toolchain", 3, Some(&[]))
        .await
        .unwrap();
    assert!(results.is_empty());
}

// ── Review 四轮回归：doc_ids 过滤检索的耗尽判定不受回填跳过的损坏行干扰 ──

#[tokio::test]
async fn document_search_widening_survives_corrupt_backfill_rows() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let engine = build_engine(&backend, MemoryNamespace::Document).await;
    let model: Arc<dyn ModelClient> = Arc::new(MockModel {
        response: String::new(),
    });
    let mut store = LocalDocumentStore::new(
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(futures::lock::Mutex::new(engine)),
        model,
    );

    // 大文档：6 个与查询同向的 chunk（均比小文档相似度高）
    let paragraph = "alpha beta gamma delta ".repeat(80);
    let big_content = vec![paragraph; 6].join("\n\n");
    let big = store.index_content("big.txt", &big_content).await.unwrap();
    assert!(big.chunk_count > 4);
    let small = store
        .index_content("small.txt", "zzzz qqqq jjjj xxxx")
        .await
        .unwrap();

    // JSON 级损坏大文档全部 chunk 的 memories 行：回填跳过后首轮
    // hits 为空（top4 全为大文档 chunk），旧耗尽判定
    // `hits.len() < fetch_k` 误判索引耗尽，提前停止扩窗返回空
    let memories = backend.table(TABLE_MEMORIES);
    for i in 0..big.chunk_count {
        memories
            .set(
                &format!("document/{}-c{i}", big.id),
                &json!(["corrupt"]),
                None,
            )
            .await
            .unwrap();
    }

    // 新耗尽判定按 namespace 总条数上界扩窗，小文档唯一 chunk 仍可命中
    let results = store
        .search(
            "alpha beta gamma delta",
            1,
            Some(std::slice::from_ref(&small.id)),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].chunk.doc_id, small.id);
}

// ── Review 五轮回归：去重刷新 metadata 以新写入为准（importance 取 max）──

#[tokio::test]
async fn refresh_replaces_metadata_with_new_write() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    engine
        .remember(
            MemoryNamespace::Personal,
            "deploy from main only",
            &embed_text("deploy from main only"),
            json!({"importance": 2.0, "tags": ["a"], "stale_key": true}),
        )
        .await
        .unwrap();
    // 同签名刷新：旧行为仅更新 importance/refreshed_at，新 metadata
    // 的其他字段（tags 替换、stale_key 移除）被静默丢弃
    let refreshed = engine
        .remember(
            MemoryNamespace::Personal,
            "Deploy from MAIN only!",
            &embed_text("Deploy from MAIN only!"),
            json!({"tags": ["b"]}),
        )
        .await
        .unwrap();

    assert_eq!(refreshed.metadata["tags"], json!(["b"]));
    assert!(refreshed.metadata.get("stale_key").is_none());
    // importance 取 max（新写入缺省 1.0 < 既有 2.0）；新鲜度锚点由引擎维护
    assert_eq!(refreshed.metadata["importance"], json!(2.0));
    assert!(refreshed.metadata.get("refreshed_at").is_some());
    // 落盘行与返回值一致
    let stored = engine
        .get(MemoryNamespace::Personal, &refreshed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.metadata, refreshed.metadata);
}

// ── Review 五轮回归：search_ranked 过采样，窗口外的新鲜条目可反超 ──

#[tokio::test]
async fn search_ranked_overfetches_beyond_raw_topk_window() {
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&dir);
    let mut engine = build_engine(&backend, MemoryNamespace::Personal).await;

    // old 与 query 完全对齐（余弦 1.0）但 60 天前刷新（衰减 0.25）；
    // fresh 偏离（余弦 ≈ 0.707）但刚刷新。top_k = 1 时原始窗口只含
    // old，无过采样则 fresh 永远无法反超（衰减只降不升）。
    let now = rust_agent::memory::now_ms();
    let sixty_days = 60 * 24 * 3600 * 1000;
    let query = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let inv = 1.0f32 / 2.0f32.sqrt();
    let fresh_vec = vec![inv, inv, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    engine
        .insert_with_id(
            MemoryNamespace::Personal,
            "old",
            "stale exact match",
            &query,
            json!({ "refreshed_at": now - sixty_days }),
        )
        .await
        .unwrap();
    engine
        .insert_with_id(
            MemoryNamespace::Personal,
            "fresh",
            "fresh near match",
            &fresh_vec,
            json!({ "refreshed_at": now }),
        )
        .await
        .unwrap();

    // 原始 top-1 窗口只含 old
    let plain = engine
        .search(MemoryNamespace::Personal, &query, 1)
        .await
        .unwrap();
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].0.id, "old");

    // 衰减重排：过采样后 fresh（0.707）反超 old（1.0 × 0.25），
    // 且结果仍截断回 top_k
    let ranked = engine
        .search_ranked(MemoryNamespace::Personal, &query, 1)
        .await
        .unwrap();
    assert_eq!(ranked.len(), 1);
    assert_eq!(
        ranked[0].0.id, "fresh",
        "overfetch must let fresh entry displace stale one beyond raw window"
    );
}

// ── Review 五轮回归：模型流未完成时抽取报告明确的 skipped 原因 ──

/// 只发 TextDelta、不发 Complete 就结束的模型流（重试耗尽场景）。
struct NoCompleteModel;

#[async_trait::async_trait]
impl ModelClient for NoCompleteModel {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        let events = vec![ModelStreamEvent::TextDelta {
            text: "partial".into(),
        }];
        Ok(futures::stream::iter(events).boxed())
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AgentError> {
        Ok(embed_text(text))
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
        Err(AgentError::Model("stt unsupported in mock".into()))
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
        Err(AgentError::Model("tts unsupported in mock".into()))
    }
}

#[tokio::test]
async fn extractor_reports_incomplete_stream() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let extractor =
        MemoryExtractor::new(MemdirStore::new(Arc::clone(&kv)), Arc::new(NoCompleteModel));

    let messages = vec![
        ConversationMessage::from_user_text("deploy only from main"),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Understood.".into(),
            }],
        },
    ];
    // 旧行为：skipped=None + saved 空，与“模型判定无可保存”无法区分
    let outcome = extractor.maybe_extract(&messages, false).await.unwrap();
    assert!(outcome.saved.is_empty());
    assert_eq!(
        outcome.skipped.as_deref(),
        Some("model stream ended without completion")
    );
    assert!(MemdirStore::new(kv).scan(10).await.unwrap().is_empty());
}

// ── Review 五轮回归：索引行锚定行尾 `]({filename})`，标题文本不误伤 ──

#[tokio::test]
async fn memdir_index_anchor_ignores_title_text() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(open_backend(&dir).table(TABLE_KV));
    let store = MemdirStore::new(Arc::clone(&kv));

    // B 的标题文本含 "(build.md)" 字样：旧 contains 判定会误认为
    // build.md 的索引行已存在（漏追加）/ 误删 B 的索引行
    store
        .add_entry(NewMemoryEntry {
            title: "see (build.md) notes".into(),
            body: "Reference to the build notes entry.".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let filename = store
        .add_entry(NewMemoryEntry {
            title: "Build".into(),
            body: "Use cargo build --workspace.".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(filename, "build.md");

    // 两条索引行都必须存在
    let index = store.read_index().await.unwrap().unwrap();
    assert!(index.lines().any(|l| l.ends_with("](build.md)")));
    assert!(
        index
            .lines()
            .any(|l| l.ends_with("](see_build_md_notes.md)"))
    );

    // 删除 build.md：只删它自己的索引行，B 的行保留
    assert!(store.remove_entry("build").await.unwrap());
    let index = store.read_index().await.unwrap().unwrap();
    assert!(!index.lines().any(|l| l.ends_with("](build.md)")));
    assert!(
        index
            .lines()
            .any(|l| l.ends_with("](see_build_md_notes.md)"))
    );
}

// ── Review 五轮回归：delete_prefix 单写事务批量删除（含过期/损坏行）──

#[tokio::test]
async fn delete_prefix_removes_expired_and_corrupt_rows() {
    let dir = tempfile::tempdir().unwrap();
    {
        let backend = open_backend(&dir);
        let kv = backend.table(TABLE_KV);
        kv.set("a/live", &json!(1), None).await.unwrap();
        kv.set("a/gone", &json!(2), Some(Duration::from_millis(1)))
            .await
            .unwrap();
        kv.set("b/other", &json!(3), None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // 绕过 Envelope 直接写入损坏行（redb 独占锁：先释放上面的句柄）
    {
        use redb::TableDefinition;
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(TABLE_KV);
        let db = redb::Database::open(dir.path().join("ains.redb")).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(def).unwrap();
            table.insert("a/corrupt", [0xFFu8; 3].as_slice()).unwrap();
        }
        write.commit().unwrap();
    }

    let kv = RedbBackend::open(dir.path().join("ains.redb"))
        .unwrap()
        .table(TABLE_KV);
    // 前缀内全部行（存活/过期/损坏）一并清除；b/ 不受影响
    assert_eq!(kv.delete_prefix("a/").await.unwrap(), 3);
    assert_eq!(kv.get("a/live").await.unwrap(), None);
    assert!(kv.list_prefix("a/").await.unwrap().is_empty());
    assert_eq!(kv.get("b/other").await.unwrap(), Some(json!(3)));
    // 幂等：再删无可删
    assert_eq!(kv.delete_prefix("a/").await.unwrap(), 0);
}
