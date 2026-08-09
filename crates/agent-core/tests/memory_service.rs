//! 生产 MemoryService 集成测试（AINS 向量表生产路径调用方设计 §20）。
//!
//! 覆盖：scoped dedupe（§20.1）、visibility（§20.2）、extraction 幂等
//! （§20.3）、checkpoint（§20.4）、metadata/manifest/TTL（§20.5）、
//! embedding contract（§20.6）、encryption table domain（§20.8）。

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use agent_core::error::{AgentError, MemoryError};
use agent_core::kernel::messages::{ContentBlock, ConversationMessage, Role};
use agent_core::memory::kv::{
    TABLE_DOCUMENTS, TABLE_EMBEDDINGS, TABLE_HNSW_CACHE, TABLE_KV, TABLE_MEMORIES,
};
use agent_core::memory::kv_crypto::EncryptionKey;
use agent_core::memory::{
    DefaultVectorIndexManager, DurableMemoryMetadata, ExtractionReason, KvStore, MemoryBackend,
    MemoryContext, MemoryEngine, MemoryEntry, MemoryNamespace, MemoryScope, MemoryService,
    MemoryServiceConfig, MemoryStores, MemoryType, Metric, NewMemoryEntry, RedbBackend,
    VectorIndexConfig, VectorIndexManager, build_durable_manifest, extract_digest, is_visible,
    now_ms, open_memory_stores, owner_key_for_id, prepare_encryption, vector_to_value,
};
use futures::StreamExt;
use std::time::Duration;

use agent_core::model_client::{
    EventStream, ModelClient, ModelRequest, ModelStreamEvent, UsageSnapshot,
};

const DIM: u32 = 8;

/// 确定性 embedding：字符直方图折叠到 8 维并归一化（与 memory_native.rs 同口径）。
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

/// 可配置抽取响应的 MockModel：`stream_response` 返回预设 JSON，
/// `embed` 返回确定性向量。
struct MockModel {
    response: String,
}

/// 模拟传输在 `Complete` 前结束。该情况不是模型显式返回空记忆，必须走
/// extraction failure/backoff，而不是写入 success digest。
struct IncompleteStreamModel;

#[async_trait::async_trait]
impl ModelClient for IncompleteStreamModel {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        Ok(futures::stream::empty().boxed())
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

impl MockModel {
    fn new(response: String) -> Self {
        Self { response }
    }

    fn memories_json(records: Vec<serde_json::Value>) -> String {
        serde_json::to_string(&json!({ "memories": records })).unwrap()
    }

    fn empty() -> Self {
        Self {
            response: Self::memories_json(vec![]),
        }
    }
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

/// `index_project_docs` 并发回归测试用：让两个 service 都有机会抵达
/// source-hash 检查前的 async 边界。共享 document gate 存在时，第二个
/// service 必须在第一个完整落盘后才会进入，因而不会重复 embed/index。
struct SlowEmbedModel {
    embed_calls: AtomicUsize,
}

impl SlowEmbedModel {
    fn new() -> Self {
        Self {
            embed_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ModelClient for SlowEmbedModel {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        Ok(futures::stream::empty().boxed())
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AgentError> {
        self.embed_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(embed_text(text))
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
        Err(AgentError::Model("stt unsupported in mock".into()))
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
        Err(AgentError::Model("tts unsupported in mock".into()))
    }
}

/// Separate restored service instances must share the session extraction gate.
/// The sleep forces both callers to reach the async extraction path; without a
/// shared gate they would both invoke the model before either writes its digest.
struct CountingSlowExtractionModel {
    extraction_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelClient for CountingSlowExtractionModel {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        self.extraction_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let message = ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: MockModel::memories_json(Vec::new()),
            }],
        };
        Ok(futures::stream::iter(vec![ModelStreamEvent::Complete {
            message,
            usage: UsageSnapshot::default(),
            stop_reason: None,
        }])
        .boxed())
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

/// 指定第 N 次 `set` 调用失败（其余调用透传）的 KvStore 包装：注入
/// 持久化/契约写入失败场景。
struct FailingKv {
    inner: Box<dyn KvStore>,
    fail_on: Vec<usize>,
    sets: std::sync::atomic::AtomicUsize,
}

impl FailingKv {
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
        let n = self.sets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_on.contains(&n) {
            return Err(MemoryError::Storage(format!("injected set failure #{n}")));
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

fn open_stores(dir: &tempfile::TempDir) -> MemoryStores {
    let backend = RedbBackend::open(dir.path().join("ains.redb")).expect("open redb");
    open_memory_stores(&MemoryBackend::Native(Arc::new(backend)), None)
}

fn context(project_key: &str, session_id: &str) -> MemoryContext {
    MemoryContext::new(project_key, session_id)
}

fn config() -> MemoryServiceConfig {
    MemoryServiceConfig {
        min_recall_score: 0.0,
        ..MemoryServiceConfig::default()
    }
}

async fn service(
    stores: MemoryStores,
    model: Arc<dyn ModelClient>,
    ctx: MemoryContext,
) -> MemoryService {
    MemoryService::new(stores, model, ctx, config())
        .await
        .expect("memory service")
}

fn record(body: &str, scope: MemoryScope) -> NewMemoryEntry {
    NewMemoryEntry {
        title: body.chars().take(20).collect(),
        body: body.to_string(),
        description: format!("desc {body}"),
        memory_type: MemoryType::Project,
        scope,
        importance: 1.0,
        source: "test".to_string(),
        ttl_days: 0,
        tags: vec!["tag".to_string()],
    }
}

fn messages(user: &str, assistant: &str) -> Vec<ConversationMessage> {
    vec![
        ConversationMessage::from_user_text(user),
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: assistant.into(),
            }],
        },
    ]
}

// ── §20.1 Scoped Dedupe ──────────────────────────────────────────

#[tokio::test]
async fn same_body_across_projects_are_independent() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let svc_b = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-b", "s1"),
    )
    .await;

    let id_a = svc_a
        .write_memory(record("alpha beta gamma", MemoryScope::Project))
        .await
        .unwrap();
    let id_b = svc_b
        .write_memory(record("alpha beta gamma", MemoryScope::Project))
        .await
        .unwrap();

    // 同正文跨 project → 两条独立 memory（互不刷新）
    assert_ne!(
        id_a, id_b,
        "跨 dedupe domain 的同正文必须使用不同 storage id，即使写入落在同一毫秒"
    );
    let hits_a = svc_a.search("alpha beta gamma", 10).await.unwrap();
    let hits_b = svc_b.search("alpha beta gamma", 10).await.unwrap();
    assert_eq!(hits_a.len(), 1);
    assert_eq!(hits_b.len(), 1);
    assert_ne!(hits_a[0].content, "");
    assert_eq!(hits_a[0].title, hits_b[0].title);
}

#[tokio::test]
async fn shared_stores_keep_cross_session_index_and_dedupe_consistent() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "session-a"),
    )
    .await;
    let svc_b = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "session-b"),
    )
    .await;

    svc_a
        .write_memory(record("initial shared memory", MemoryScope::Project))
        .await
        .unwrap();
    // 先让 A materialize Personal index；修复前它会永久看不到 B 后写入的
    // embedding，因为每个 service 都有独立 Loaded index。
    assert_eq!(
        svc_a
            .search("initial shared memory", 5)
            .await
            .unwrap()
            .len(),
        1
    );
    svc_b
        .write_memory(record("new memory from session b", MemoryScope::Project))
        .await
        .unwrap();
    assert_eq!(
        svc_a.revision(),
        svc_b.revision(),
        "跨 session 写入必须使所有 provider 观察到同一个 cache revision"
    );
    assert!(
        svc_a
            .search("new memory from session b", 5)
            .await
            .unwrap()
            .iter()
            .any(|hit| hit.content == "new memory from session b"),
        "已 materialize 的 session A 索引必须即时接收 session B 写入"
    );

    // 同一 dedupe domain 的并发写入必须线性化为一次创建 + 一次 refresh。
    let (left, right) = tokio::join!(
        svc_a.write_memory(record("one shared fact", MemoryScope::Project)),
        svc_b.write_memory(record("one shared fact", MemoryScope::Project)),
    );
    assert_eq!(left.unwrap(), right.unwrap());
    let shared = svc_a.search("one shared fact", 10).await.unwrap();
    assert_eq!(
        shared
            .iter()
            .filter(|hit| hit.content == "one shared fact")
            .count(),
        1,
        "跨 session scoped dedupe 不得产生 orphan duplicate"
    );
}

#[tokio::test]
async fn private_and_project_same_body_are_independent() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    svc.write_memory(record("shared body text", MemoryScope::Private))
        .await
        .unwrap();
    svc.write_memory(record("shared body text", MemoryScope::Project))
        .await
        .unwrap();

    // Private 与 Project 相同正文 → 两条独立 memory
    let hits = svc.search("shared body text", 10).await.unwrap();
    assert_eq!(hits.len(), 2);
}

#[tokio::test]
async fn same_project_same_body_refreshes_not_duplicates() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    svc.write_memory(record("stable fact body", MemoryScope::Project))
        .await
        .unwrap();
    svc.write_memory(record("stable fact body", MemoryScope::Project))
        .await
        .unwrap();

    let hits = svc.search("stable fact body", 10).await.unwrap();
    assert_eq!(hits.len(), 1, "同 project 相同正文应刷新而非重复新增");
}

#[tokio::test]
async fn team_scopes_are_independent() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let mut ctx_a = context("proj-a", "s1");
    ctx_a.team_id = Some("team-1".to_string());
    let mut ctx_b = context("proj-b", "s1");
    ctx_b.team_id = Some("team-2".to_string());
    let svc_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        ctx_a,
    )
    .await;
    let svc_b = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        ctx_b,
    )
    .await;

    svc_a
        .write_memory(record("team fact xyz", MemoryScope::Team))
        .await
        .unwrap();
    svc_b
        .write_memory(record("team fact xyz", MemoryScope::Team))
        .await
        .unwrap();

    let hits_a = svc_a.search("team fact xyz", 10).await.unwrap();
    let hits_b = svc_b.search("team fact xyz", 10).await.unwrap();
    assert_eq!(hits_a.len(), 1);
    assert_eq!(hits_b.len(), 1);
}

#[tokio::test]
async fn team_write_without_team_context_fails_closed() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    let err = svc
        .write_memory(record("team only fact", MemoryScope::Team))
        .await;
    assert!(err.is_err(), "无 team context 时 Team 写入必须 fail closed");
    // 不降级为 Private：不得写入任何条目
    let hits = svc.search("team only fact", 10).await.unwrap();
    assert!(hits.is_empty());
}

// ── §20.2 Visibility ─────────────────────────────────────────────

#[tokio::test]
async fn project_a_recall_never_returns_project_b() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let svc_b = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-b", "s1"),
    )
    .await;

    svc_a
        .write_memory(record("project a secret detail", MemoryScope::Project))
        .await
        .unwrap();
    svc_b
        .write_memory(record("project b secret detail", MemoryScope::Project))
        .await
        .unwrap();

    let hits_a = svc_a.search("secret detail", 10).await.unwrap();
    let hits_b = svc_b.search("secret detail", 10).await.unwrap();
    assert_eq!(hits_a.len(), 1);
    assert!(hits_a[0].content.contains("project a"));
    assert_eq!(hits_b.len(), 1);
    assert!(hits_b[0].content.contains("project b"));
}

#[tokio::test]
async fn private_memory_visible_across_projects_for_same_local_user() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let svc_b = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-b", "s1"),
    )
    .await;

    svc_a
        .write_memory(record("personal preference fact", MemoryScope::Private))
        .await
        .unwrap();

    let hits_b = svc_b.search("personal preference fact", 10).await.unwrap();
    assert_eq!(hits_b.len(), 1, "Private 在同一本地用户不同项目可见");
}

#[tokio::test]
async fn team_memory_visible_only_on_team_match() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let mut writer_ctx = context("proj-a", "s1");
    writer_ctx.team_id = Some("team-1".to_string());
    let writer = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        writer_ctx,
    )
    .await;
    let mut reader_match = context("proj-b", "s2");
    reader_match.team_id = Some("team-1".to_string());
    let reader_ok = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        reader_match,
    )
    .await;
    let mut reader_mismatch = context("proj-c", "s3");
    reader_mismatch.team_id = Some("team-2".to_string());
    let reader_bad = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        reader_mismatch,
    )
    .await;
    let reader_no_team = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-d", "s4"),
    )
    .await;

    writer
        .write_memory(record("team internal fact", MemoryScope::Team))
        .await
        .unwrap();

    assert_eq!(
        reader_ok
            .search("team internal fact", 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        reader_bad
            .search("team internal fact", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        reader_no_team
            .search("team internal fact", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

// ── §20.3 Extraction Idempotence ─────────────────────────────────

async fn extraction_service(stores: MemoryStores, model: Arc<MockModel>) -> MemoryService {
    service(stores, model, context("proj-a", "sess-1")).await
}

#[tokio::test]
async fn duplicate_transcript_digest_extracts_once() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::new(MockModel::memories_json(vec![json!({
        "title": "T", "content": "durable fact alpha", "scope": "project", "type": "project"
    })])));
    let svc = extraction_service(stores.clone(), Arc::clone(&model)).await;

    let msgs = messages("user prompt", "assistant answer");
    let first = svc
        .extract_durable(msgs.clone(), ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert!(first.skipped.is_none());
    assert_eq!(first.saved.len(), 1);

    // 同 transcript digest 重复 final event → skip
    let second = svc
        .extract_durable(msgs, ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert_eq!(
        second.skipped.as_deref(),
        Some("duplicate transcript digest")
    );

    // 只有一条 durable memory
    let hits = svc.search("durable fact alpha", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn sensitive_memory_records_are_not_persisted_or_injected() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "sess-1"),
    )
    .await;

    for (field, value) in [
        ("body", "api_key=sk-123456789"),
        ("body", "my password correcthorsebatterystaple"),
        ("title", "password hunter2"),
        ("description", "Bearer abcdefghijklmnop"),
        ("tag", "-----BEGIN PRIVATE KEY-----"),
    ] {
        let mut entry = record("safe durable fact", MemoryScope::Project);
        match field {
            "body" => entry.body = value.to_string(),
            "title" => entry.title = value.to_string(),
            "description" => entry.description = value.to_string(),
            "tag" => entry.tags = vec![value.to_string()],
            _ => unreachable!(),
        }
        let result = svc.write_memory(entry).await;
        assert!(
            matches!(result, Err(MemoryError::SensitiveContent)),
            "{field} must be screened before durable persistence: {result:?}"
        );
    }

    assert!(
        stores
            .memories
            .list_prefix("personal/")
            .await
            .unwrap()
            .is_empty(),
        "rejected content must not create a memory row"
    );
    assert!(
        stores
            .embeddings
            .list_prefix("personal/")
            .await
            .unwrap()
            .is_empty(),
        "rejected content must not create an embedding row"
    );
    assert!(
        svc.memory_prompt("api key", 5).await.is_empty(),
        "rejected content must never be injected into a future prompt"
    );
}

#[tokio::test]
async fn extraction_skips_sensitive_record_and_keeps_safe_records() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::new(MockModel::memories_json(vec![
        json!({
            "title": "credential",
            "content": "token=abcdef1234567890",
            "scope": "project",
            "type": "project"
        }),
        json!({
            "title": "safe fact",
            "content": "the project uses cargo test for validation",
            "scope": "project",
            "type": "project"
        }),
    ])));
    let svc = extraction_service(stores.clone(), Arc::clone(&model)).await;

    let outcome = svc
        .extract_durable(
            messages("please remember these facts", "acknowledged"),
            ExtractionReason::FinalTurn,
        )
        .await
        .expect("a rejected secret must not fail the whole extraction batch");
    assert_eq!(outcome.saved.len(), 1);
    let prompt = svc.memory_prompt("cargo validation", 5).await;
    assert!(prompt.contains("cargo test for validation"));
    assert!(!prompt.contains("abcdef1234567890"));
}

#[tokio::test]
async fn compact_and_final_snapshots_both_extract_when_digests_differ() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::new(MockModel::memories_json(vec![json!({
        "title": "T", "content": "snapshot fact beta", "scope": "project", "type": "project"
    })])));
    let svc = extraction_service(stores.clone(), Arc::clone(&model)).await;

    // compact snapshot A
    let a = messages("user a", "assistant a");
    let out_a = svc
        .extract_durable(a.clone(), ExtractionReason::Compaction)
        .await
        .unwrap();
    assert!(out_a.skipped.is_none());
    // final snapshot B（内容不同 → 新 digest；即使间隔 <30 秒也允许执行）
    let b = messages("user a", "assistant b");
    let out_b = svc
        .extract_durable(b.clone(), ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert!(out_b.skipped.is_none());
    // digest 不同 → 两次均执行
    let hits = svc.search("snapshot fact beta", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn failure_digest_backoff_only_blocks_same_digest() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    // 模型流不返回 Complete → extraction 视为失败（无响应文本）
    let model = Arc::new(MockModel {
        response: "not json at all".to_string(),
    });
    let svc = extraction_service(stores.clone(), Arc::clone(&model)).await;

    let msgs = messages("user", "assistant");
    // parse 返回空 → 正常 outcome（非失败）；改用 embed 失败注入？
    // 空 records 也属于正常 outcome（parse 返回空，§17），因此用响应 JSON
    // 无 memories 模拟“无可保存”路径。
    let out = svc
        .extract_durable(msgs.clone(), ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert!(out.skipped.is_none());
    assert!(out.saved.is_empty());

    // 重复同 digest → success digest 已记录 → skip
    let second = svc
        .extract_durable(msgs.clone(), ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert_eq!(
        second.skipped.as_deref(),
        Some("duplicate transcript digest")
    );
}

#[tokio::test]
async fn incomplete_extraction_stream_records_failure_and_does_not_poison_success_digest() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let config = MemoryServiceConfig {
        // Make the second call prove that this digest remains retryable without
        // waiting for the production backoff interval.
        extract_retry_backoff_ms: 0,
        ..config()
    };
    let svc = MemoryService::new(
        stores.clone(),
        Arc::new(IncompleteStreamModel) as Arc<dyn ModelClient>,
        context("proj-a", "sess-1"),
        config,
    )
    .await
    .unwrap();
    let msgs = messages("user", "assistant");

    let first = svc
        .extract_durable(msgs.clone(), ExtractionReason::FinalTurn)
        .await;
    assert!(
        first.is_err(),
        "incomplete stream must be an extraction failure"
    );
    assert!(
        stores
            .kv
            .get(&format!(
                "memory/status/{}/proj-a/sess-1/last_success_digest",
                owner_key_for_id("local")
            ))
            .await
            .unwrap()
            .is_none(),
        "incomplete stream must not mark the transcript successful"
    );
    assert!(
        stores
            .kv
            .get(&format!(
                "memory/status/{}/proj-a/sess-1/last_failure_digest",
                owner_key_for_id("local")
            ))
            .await
            .unwrap()
            .is_some(),
        "incomplete stream must persist the retry state"
    );

    assert!(
        svc.extract_durable(msgs, ExtractionReason::FinalTurn)
            .await
            .is_err(),
        "the same transcript must retry once its backoff permits it"
    );
}

#[tokio::test]
async fn extraction_digest_uses_session_and_transcript() {
    let d1 = extract_digest("s1", "hello");
    let d2 = extract_digest("s2", "hello");
    let d3 = extract_digest("s1", "world");
    assert_ne!(d1, d2);
    assert_ne!(d1, d3);
    assert_eq!(d1, extract_digest("s1", "hello"));
}

#[tokio::test]
async fn team_record_without_context_skipped_not_aborting_batch() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::new(MockModel::memories_json(vec![
        json!({"title": "T1", "content": "project fact gamma", "scope": "project", "type": "project"}),
        json!({"title": "T2", "content": "team fact", "scope": "team", "type": "project"}),
    ])));
    let svc = extraction_service(stores.clone(), Arc::clone(&model)).await;

    let msgs = messages("user prompt", "assistant answer");
    let outcome = svc
        .extract_durable(msgs.clone(), ExtractionReason::FinalTurn)
        .await
        .expect("抽取不得因 team 记录整批失败");
    // §3.2：无 team context 时 team 记录 skip，同批 project 记录正常写入
    assert_eq!(outcome.saved.len(), 1, "仅 project 记录写入");
    let hits = svc.search("project fact gamma", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    // 直方图 embedding 下相近正文可能互相命中，直接扫描表确认无 team 行
    let mut contents = Vec::new();
    for key in stores.memories.list_prefix("personal/").await.unwrap() {
        if let Some(raw) = stores.memories.get(&key).await.unwrap()
            && let Ok(entry) = serde_json::from_value::<MemoryEntry>(raw)
        {
            contents.push(entry.content);
        }
    }
    assert_eq!(
        contents,
        vec!["project fact gamma".to_string()],
        "team 记录不得降级写入"
    );

    // 抽取视为成功：同 digest 重触发是 success skip（而非 failure backoff）
    let second = svc
        .extract_durable(msgs, ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert_eq!(
        second.skipped.as_deref(),
        Some("duplicate transcript digest"),
        "team skip 不得污染 success digest"
    );
}

// ── §20.4 Checkpoint ─────────────────────────────────────────────

#[tokio::test]
async fn checkpoint_keys_are_owner_project_and_session_scoped() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());

    let svc_a1 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let svc_a2 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s2"),
    )
    .await;
    let svc_b1 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-b", "s1"),
    )
    .await;

    assert_ne!(svc_a1.checkpoint_key(), svc_a2.checkpoint_key());
    assert_ne!(svc_a1.checkpoint_key(), svc_b1.checkpoint_key());
    assert!(svc_a1.checkpoint_key().contains("proj-a"));

    let owner_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        MemoryContext::for_owner("proj-a", "s1", "owner-a"),
    )
    .await;
    let owner_b = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        MemoryContext::for_owner("proj-a", "s1", "owner-b"),
    )
    .await;
    assert_ne!(
        owner_a.checkpoint_key(),
        owner_b.checkpoint_key(),
        "same project/session must not share checkpoints across Web owners"
    );
}

#[tokio::test]
async fn set_session_id_updates_checkpoint_and_digest_scoping() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "session"),
    )
    .await;

    // 新会话装配时以占位 session_id 创建（修复前：checkpoint 跨会话共享）
    let placeholder_key = svc.checkpoint_key();
    assert!(placeholder_key.ends_with("/session.md"));
    // 首轮 save_snapshot 生成稳定 id 后同步（§3：与 SessionStore 同一 id）
    svc.set_session_id("real-001");
    let real_key = svc.checkpoint_key();
    assert!(real_key.ends_with("/real-001.md"));

    svc.save_checkpoint(&messages("hello there", "hi!"), None)
        .await
        .unwrap();
    let real = stores.kv.get(&real_key).await.unwrap();
    assert!(real.is_some(), "checkpoint 必须写入同步后的 session key");
    let placeholder = stores.kv.get(&placeholder_key).await.unwrap();
    assert!(placeholder.is_none(), "占位 session key 不得再写入");

    // digest 随 session_id 变化：同 transcript 在新 session 下是新 digest
    let d_old = extract_digest("session", "transcript");
    let d_new = extract_digest("real-001", "transcript");
    assert_ne!(d_old, d_new);
}

#[tokio::test]
async fn checkpoint_without_metadata_generates_recent_conversation() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    let msgs = messages("hello there", "hi!");
    svc.save_checkpoint(&msgs, None).await.unwrap();
    let doc = svc
        .load_checkpoint()
        .await
        .unwrap()
        .expect("checkpoint exists");
    assert!(doc.contains("# Session Memory"));
    assert!(doc.contains("Recent Conversation"));
    assert!(doc.contains("hello there"));
    // 不宣称 P1 拥有完整结构化 Task state
    assert!(doc.contains("Current State"));
}

#[tokio::test]
async fn later_checkpoint_preserves_the_latest_conversation() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores,
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    // A compaction checkpoint is older than the terminal-turn checkpoint.  The
    // caller must write them in this order so recovery retains the final turn.
    svc.save_checkpoint(&messages("before compact", "summary"), None)
        .await
        .unwrap();
    svc.save_checkpoint(&messages("latest user input", "final answer"), None)
        .await
        .unwrap();

    let document = svc.load_checkpoint().await.unwrap().unwrap();
    assert!(document.contains("latest user input"));
    assert!(document.contains("final answer"));
    assert!(!document.contains("before compact"));
}

#[tokio::test]
async fn checkpoint_failure_persists_scoped_error_status() {
    let dir = tempfile::TempDir::new().unwrap();
    let backend = RedbBackend::open(dir.path().join("ains.redb")).expect("open redb");
    let raw_kv = backend.table(TABLE_KV);
    let failing_kv: Arc<dyn KvStore> =
        Arc::new(FailingKv::failing_at(backend.table(TABLE_KV), vec![0]));
    let stores = MemoryStores::from_parts(
        failing_kv,
        Arc::new(backend.table(TABLE_MEMORIES)),
        Arc::new(backend.table(TABLE_EMBEDDINGS)),
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(backend.table(TABLE_HNSW_CACHE)),
    );
    let svc = service(
        stores,
        Arc::new(MockModel::empty()) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    assert!(
        svc.save_checkpoint(&messages("user", "assistant"), None)
            .await
            .is_err()
    );
    let status = raw_kv
        .get(&format!(
            "memory/status/{}/proj-a/s1/checkpoint_last_error",
            owner_key_for_id("local")
        ))
        .await
        .unwrap()
        .and_then(|value| value.as_str().map(str::to_owned));
    assert!(
        status
            .as_ref()
            .is_some_and(|message| message.contains("injected set failure")),
        "checkpoint failure must be observable under its scoped status key: {status:?}"
    );
}

// ── §20.5 Metadata / Manifest / TTL ──────────────────────────────

#[tokio::test]
async fn metadata_fields_are_preserved_and_manifest_reads_title_description() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    let mut entry = record("rich metadata body", MemoryScope::Project);
    entry.title = "my title".to_string();
    entry.description = "my description".to_string();
    entry.tags = vec!["a".to_string(), "b".to_string()];
    entry.ttl_days = 0;
    svc.write_memory(entry).await.unwrap();

    // 落盘 metadata 完整保留
    let mut raw = None;
    for key in stores.memories.list_prefix("personal/").await.unwrap() {
        if let Some(value) = stores.memories.get(&key).await.unwrap() {
            let meta: DurableMemoryMetadata =
                serde_json::from_value(value.get("metadata").unwrap().clone()).unwrap();
            assert_eq!(meta.schema_version, DurableMemoryMetadata::SCHEMA_VERSION);
            assert_eq!(meta.title, "my title");
            assert_eq!(meta.description, "my description");
            assert_eq!(meta.tags, vec!["a".to_string(), "b".to_string()]);
            assert_eq!(meta.scope, MemoryScope::Project);
            assert_eq!(meta.project_key.as_deref(), Some("proj-a"));
            assert!(meta.dedupe_domain.starts_with("personal:project:"));
            raw = Some(value);
        }
    }
    assert!(raw.is_some(), "at least one memory row");
}

#[tokio::test]
async fn expired_memory_is_filtered_from_recall() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    let mut entry = record("expiring fact", MemoryScope::Private);
    entry.ttl_days = 1;
    svc.write_memory(entry).await.unwrap();

    // 过期（expires_at 已到）→ 不进入 recall
    let mut raw = None;
    for key in stores.memories.list_prefix("personal/").await.unwrap() {
        if let Some(value) = stores.memories.get(&key).await.unwrap() {
            raw = Some(value);
        }
    }
    let value = raw.expect("row exists");
    let mut meta: DurableMemoryMetadata =
        serde_json::from_value(value.get("metadata").unwrap().clone()).unwrap();
    // 把 expires_at 拨到过去，模拟已过期
    meta.expires_at_ms = Some(0);
    let key = value.get("id").unwrap().as_str().unwrap().to_string();
    let mut entry_json = value.clone();
    entry_json["metadata"] = serde_json::to_value(&meta).unwrap();
    stores
        .memories
        .set(&format!("personal/{key}"), &entry_json, None)
        .await
        .unwrap();

    let hits = svc.search("expiring fact", 10).await.unwrap();
    assert!(hits.is_empty(), "TTL 过期项不得进入 prompt");
}

#[tokio::test]
async fn memory_prompt_reports_earliest_recalled_ttl_for_cache_invalidation() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    let mut entry = record("short lived cache fact", MemoryScope::Private);
    entry.ttl_days = 1;
    svc.write_memory(entry).await.unwrap();

    let key = stores
        .memories
        .list_prefix("personal/")
        .await
        .unwrap()
        .into_iter()
        .find(|key| key.starts_with("personal/mem-"))
        .expect("durable memory entry");
    let mut raw = stores.memories.get(&key).await.unwrap().unwrap();
    let mut meta: DurableMemoryMetadata =
        serde_json::from_value(raw.get("metadata").unwrap().clone()).unwrap();
    let expires_at_ms = now_ms().saturating_add(60_000);
    meta.expires_at_ms = Some(expires_at_ms);
    raw["metadata"] = serde_json::to_value(meta).unwrap();
    stores.memories.set(&key, &raw, None).await.unwrap();

    let (prompt, earliest_expiry) = svc
        .memory_prompt_with_expiry("short lived cache fact", 5)
        .await;
    assert!(prompt.contains("short lived cache fact"));
    assert_eq!(earliest_expiry, Some(expires_at_ms));
}

#[tokio::test]
async fn maximum_ttl_saturates_without_overflow() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    let mut entry = record("long-lived fact", MemoryScope::Private);
    entry.ttl_days = i64::MAX;
    svc.write_memory(entry).await.unwrap();

    let key = stores
        .memories
        .list_prefix("personal/")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let value = stores.memories.get(&key).await.unwrap().unwrap();
    let meta: DurableMemoryMetadata =
        serde_json::from_value(value.get("metadata").unwrap().clone()).unwrap();
    assert_eq!(meta.expires_at_ms, Some(i64::MAX));
}

// ── §20.6 Embedding Contract ─────────────────────────────────────

#[tokio::test]
async fn first_embed_establishes_contract_and_restart_reuses_dimension() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());

    let svc1 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    svc1.write_memory(record("contract body", MemoryScope::Private))
        .await
        .unwrap();

    let contract_raw = stores
        .kv
        .get("memory/embedding_contract")
        .await
        .unwrap()
        .expect("contract persisted");
    let dim = contract_raw.get("dimension").unwrap().as_u64().unwrap();
    assert_eq!(dim, DIM as u64);
    drop(svc1);

    // 重启（新 service 实例，同一 stores）：从 contract 重建 Personal index，
    // recall 立即可用
    let svc2 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let hits = svc2.search("contract body", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn dimension_mismatch_fails_closed() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    // 先写入契约（8 维），再用不同维度模型写入 → 必须 fail closed
    let model = Arc::new(MockModel::empty());
    let svc1 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    svc1.write_memory(record("seed body", MemoryScope::Private))
        .await
        .unwrap();
    drop(svc1);

    // 不同维度模型：embed 返回 16 维
    struct Dim16Model;
    #[async_trait::async_trait]
    impl ModelClient for Dim16Model {
        async fn stream_response(
            &self,
            _request: ModelRequest,
        ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
            Err(AgentError::Model("unused".into()))
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, AgentError> {
            Ok(vec![0.1f32; 16])
        }
        async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
            Err(AgentError::Model("stt unsupported".into()))
        }
        async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
            Err(AgentError::Model("tts unsupported".into()))
        }
    }
    let svc2 = service(
        stores.clone(),
        Arc::new(Dim16Model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let err = svc2
        .write_memory(record("new body", MemoryScope::Private))
        .await;
    assert!(err.is_err(), "dimension mismatch 必须 fail closed");
    let err = svc2.search("seed body", 10).await;
    assert!(
        err.is_err(),
        "dimension mismatch 的 recall 必须 fail closed"
    );
}

// ── §20.8 Encryption table domain ────────────────────────────────

#[tokio::test]
async fn ciphertext_cannot_cross_table_domains() {
    use agent_core::error::MemoryError;
    use agent_core::memory::{EncryptionKey, TABLE_EMBEDDINGS, TABLE_MEMORIES};

    let dir = tempfile::TempDir::new().unwrap();
    let backend = RedbBackend::open(dir.path().join("ains.redb")).unwrap();
    let raw_memories = backend.table(TABLE_MEMORIES);
    let raw_embeddings = backend.table(TABLE_EMBEDDINGS);
    let stores = open_memory_stores(
        &MemoryBackend::Native(Arc::new(backend)),
        Some(EncryptionKey::from_bytes([1u8; 32])),
    );

    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    svc.write_memory(record("encrypted body", MemoryScope::Private))
        .await
        .unwrap();

    // 找到实际写入的 storage key
    let real_key = stores
        .memories
        .list_prefix("personal/")
        .await
        .unwrap()
        .pop()
        .expect("at least one row");
    // 经 memories 域 wrapper 可读（AAD = memories\0key）
    assert!(stores.memories.get(&real_key).await.unwrap().is_some());

    // 底层密文（memories 域加密信封）搬到 embeddings 表同 key
    let sealed = raw_memories
        .get(&real_key)
        .await
        .unwrap()
        .expect("raw sealed row");
    raw_embeddings.set(&real_key, &sealed, None).await.unwrap();
    // 经 embeddings 域 wrapper（AAD = embeddings\0key）读取 → 认证失败
    let cross = stores.embeddings.get(&real_key).await;
    assert!(
        matches!(cross, Err(MemoryError::Encryption(_))),
        "跨表密文搬运必须认证失败, got {cross:?}"
    );

    // kv 表（legacy 兼容模式，AAD = storage_key）仍可正常读写
    stores
        .kv
        .set("legacy/key", &json!({ "v": 1 }), None)
        .await
        .unwrap();
    assert_eq!(
        stores.kv.get("legacy/key").await.unwrap(),
        Some(json!({ "v": 1 }))
    );
}

#[tokio::test]
async fn unreadable_encrypted_embedding_does_not_disable_valid_recall() {
    let dir = tempfile::TempDir::new().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("ains.redb")).unwrap());
    let stores = open_memory_stores(
        &MemoryBackend::Native(Arc::clone(&backend)),
        Some(EncryptionKey::from_bytes([7u8; 32])),
    );
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    svc.write_memory(record("valid encrypted recall fact", MemoryScope::Private))
        .await
        .unwrap();

    // Bypass the encryption wrapper to simulate one damaged embedding row.
    // The index loader must skip it and still materialize valid entries.
    backend
        .table(TABLE_EMBEDDINGS)
        .set(
            "personal/tampered",
            &json!({ "not": "a sealed envelope" }),
            None,
        )
        .await
        .unwrap();

    let hits = svc.search("valid encrypted recall fact", 5).await.unwrap();
    assert!(
        hits.iter()
            .any(|hit| hit.content == "valid encrypted recall fact"),
        "one unreadable embedding must not disable all recall"
    );
}

#[tokio::test]
async fn encryption_refuses_plaintext_until_explicit_reset() {
    let dir = tempfile::TempDir::new().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("ains.redb")).unwrap());
    backend
        .table(TABLE_KV)
        .set("legacy/plaintext", &json!("old session data"), None)
        .await
        .unwrap();
    let memory_backend = MemoryBackend::Native(Arc::clone(&backend));

    let key = EncryptionKey::from_bytes([3u8; 32]);
    let err = prepare_encryption(&memory_backend, &key, false)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("plaintext"),
        "明文库不能直接套加密 wrapper: {err}"
    );
    assert!(
        backend
            .table(TABLE_KV)
            .get("legacy/plaintext")
            .await
            .unwrap()
            .is_some(),
        "拒绝转换时不得修改既有数据"
    );

    prepare_encryption(&memory_backend, &key, true)
        .await
        .unwrap();
    assert!(
        backend
            .table(TABLE_KV)
            .get("legacy/plaintext")
            .await
            .unwrap()
            .is_none(),
        "只有显式 reset 才可清空旧明文数据"
    );
    let stores = open_memory_stores(&memory_backend, Some(EncryptionKey::from_bytes([3u8; 32])));
    stores
        .kv
        .set("new/encrypted", &json!("new data"), None)
        .await
        .unwrap();
    assert_eq!(
        stores.kv.get("new/encrypted").await.unwrap(),
        Some(json!("new data"))
    );
}

#[tokio::test]
async fn encryption_rejects_unreadable_sealed_rows_and_reset_removes_them() {
    let dir = tempfile::TempDir::new().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("ains.redb")).unwrap());
    let memory_backend = MemoryBackend::Native(Arc::clone(&backend));
    let stores_a = open_memory_stores(&memory_backend, Some(EncryptionKey::from_bytes([4u8; 32])));
    stores_a
        .kv
        .set("encrypted/old", &json!("old encrypted data"), None)
        .await
        .unwrap();

    let key_b = EncryptionKey::from_bytes([5u8; 32]);
    let err = prepare_encryption(&memory_backend, &key_b, false)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("unreadable encrypted tables"),
        "错误 key / 旧 AAD 的密文必须在装配前 fail closed: {err}"
    );
    prepare_encryption(&memory_backend, &key_b, true)
        .await
        .unwrap();
    assert!(
        backend
            .table(TABLE_KV)
            .get("encrypted/old")
            .await
            .unwrap()
            .is_none(),
        "显式 reset 必须同样清除不可解密的密文行"
    );
}

#[tokio::test]
async fn kv_legacy_and_table_domain_modes_coexist() {
    use agent_core::error::MemoryError;
    use agent_core::memory::{EncryptionKey, TABLE_EMBEDDINGS, TABLE_MEMORIES};

    let dir = tempfile::TempDir::new().unwrap();
    let backend = RedbBackend::open(dir.path().join("ains.redb")).unwrap();
    let raw_memories = backend.table(TABLE_MEMORIES);
    let raw_embeddings = backend.table(TABLE_EMBEDDINGS);
    let stores = open_memory_stores(
        &MemoryBackend::Native(Arc::new(backend)),
        Some(EncryptionKey::from_bytes([2u8; 32])),
    );

    // memories 表（table domain）正常读写
    stores
        .memories
        .set("personal/mem-1", &json!({ "content": "secret" }), None)
        .await
        .unwrap();
    assert_eq!(
        stores.memories.get("personal/mem-1").await.unwrap(),
        Some(json!({ "content": "secret" }))
    );
    // 底层密文搬到 embeddings 表（同 storage key）→ 经 embeddings 域读取认证失败
    let sealed = raw_memories.get("personal/mem-1").await.unwrap().unwrap();
    raw_embeddings
        .set("personal/mem-1", &sealed, None)
        .await
        .unwrap();
    let cross = stores.embeddings.get("personal/mem-1").await;
    assert!(
        matches!(cross, Err(MemoryError::Encryption(_))),
        "跨表搬运必须认证失败, got {cross:?}"
    );
}

// ── §20.7 Dynamic Recall（服务级）────────────────────────────────

#[tokio::test]
async fn new_session_first_turn_recalls_private_memory() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    // session 1 写入 Private memory
    let svc1 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    svc1.write_memory(record("recallable private fact", MemoryScope::Private))
        .await
        .unwrap();
    drop(svc1);
    // 新 session（恢复场景）：首轮即可 recall
    let svc2 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s2"),
    )
    .await;
    let hits = svc2.search("recallable private fact", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn turn_n_written_memory_recalled_on_turn_n_plus_1() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    // Turn N 写入
    svc.write_memory(record("next turn fact", MemoryScope::Private))
        .await
        .unwrap();
    // Turn N+1 可召回
    let hits = svc.search("next turn fact", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
}

// ── §20 补齐：legacy forget / visibility fail-closed / backoff / gate / TTL refresh / profile ──

#[tokio::test]
async fn legacy_memory_without_dedupe_domain_forgets_via_v1_signature() {
    use agent_core::memory::manage::content_signature;
    use agent_core::memory::{TABLE_EMBEDDINGS, TABLE_HNSW_CACHE, TABLE_MEMORIES};
    let dir = tempfile::TempDir::new().unwrap();
    let backend = RedbBackend::open(dir.path().join("ains.redb")).unwrap();
    let memories: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_MEMORIES));
    let embeddings: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_EMBEDDINGS));
    let mut manager = DefaultVectorIndexManager::new(
        Arc::clone(&embeddings),
        Arc::new(backend.table(TABLE_HNSW_CACHE)) as Arc<dyn KvStore>,
    );
    manager
        .create_index(
            MemoryNamespace::Personal,
            VectorIndexConfig {
                dimension: DIM,
                distance_metric: Metric::Cosine,
                m: 16,
                ef: 50,
            },
        )
        .await
        .unwrap();
    let mut engine = MemoryEngine::new(
        Arc::clone(&memories),
        Arc::clone(&embeddings),
        Box::new(manager),
    );

    // 手工构造 legacy 数据（旧版本产物）：v1 签名行 + 无 dedupe_domain 的 entry
    let content = "legacy fact body";
    let sig = content_signature(content, "personal");
    let v1_key = format!("sig/personal/{sig}");
    let id = "mem-legacy-1";
    let entry_key = format!("personal/{id}");
    let entry = MemoryEntry {
        id: id.into(),
        content: content.into(),
        namespace: MemoryNamespace::Personal,
        metadata: json!({ "importance": 1.0 }),
        created_at: now_ms(),
    };
    memories
        .set(&entry_key, &serde_json::to_value(&entry).unwrap(), None)
        .await
        .unwrap();
    memories.set(&v1_key, &json!(id), None).await.unwrap();
    embeddings
        .set(&entry_key, &vector_to_value(&embed_text(content)), None)
        .await
        .unwrap();

    // forget → legacy v1 签名行被清理（兼容路径，不残留）
    engine.forget(MemoryNamespace::Personal, id).await.unwrap();
    assert!(memories.get(&v1_key).await.unwrap().is_none());
    assert!(memories.get(&entry_key).await.unwrap().is_none());
}

#[tokio::test]
async fn visibility_fails_closed_on_missing_identity() {
    let ctx = context("proj-a", "s1");
    let mut ctx_team = context("proj-a", "s1");
    ctx_team.team_id = Some("team-1".to_string());
    // Project 缺 project_key → fail closed
    let meta = DurableMemoryMetadata::from_record(
        &record("body", MemoryScope::Project),
        MemoryScope::Project,
        None,
        None,
        "personal:project:x".into(),
        "s1",
        &ctx.owner_key,
    );
    assert!(
        !is_visible(&meta, &ctx),
        "Project 缺 project_key 必须 fail closed"
    );
    // Team 缺 team_id → fail closed（即使当前 ctx 有 team_id）
    let meta = DurableMemoryMetadata::from_record(
        &record("body", MemoryScope::Team),
        MemoryScope::Team,
        None,
        None,
        "personal:team:x".into(),
        "s1",
        &ctx_team.owner_key,
    );
    assert!(
        !is_visible(&meta, &ctx_team),
        "Team 缺 team_id 必须 fail closed"
    );
}

#[tokio::test]
async fn private_and_project_memory_are_isolated_by_owner() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let ctx_a = MemoryContext::for_owner("same-project", "s1", "user-a");
    let ctx_b = MemoryContext::for_owner("same-project", "s1", "user-b");
    assert_ne!(ctx_a.owner_key, ctx_b.owner_key);
    let svc_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        ctx_a,
    )
    .await;
    let svc_b = service(stores, Arc::clone(&model) as Arc<dyn ModelClient>, ctx_b).await;

    svc_a
        .write_memory(record("account A private fact", MemoryScope::Private))
        .await
        .unwrap();
    svc_a
        .write_memory(record("account A project fact", MemoryScope::Project))
        .await
        .unwrap();

    assert!(
        svc_b
            .search("account A private fact", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        svc_b
            .search("account A project fact", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// 流式请求直接失败的模型（注入 extraction 失败）。
struct FailingModel;

#[async_trait::async_trait]
impl ModelClient for FailingModel {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        Err(AgentError::Model("injected failure".into()))
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, AgentError> {
        Ok(vec![0.1f32; DIM as usize])
    }
    async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
        Err(AgentError::Model("stt unsupported".into()))
    }
    async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
        Err(AgentError::Model("tts unsupported".into()))
    }
}

#[tokio::test]
async fn same_digest_failure_retry_respects_backoff() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let svc = service(
        stores.clone(),
        Arc::new(FailingModel) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let msgs = messages("user", "assistant");
    // 首次失败：记录 failure digest/time
    let err = svc
        .extract_durable(msgs.clone(), ExtractionReason::FinalTurn)
        .await;
    assert!(err.is_err(), "注入的模型失败必须上抛");
    // backoff 内同 digest 重触发 → skip retry
    let out = svc
        .extract_durable(msgs, ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert_eq!(
        out.skipped.as_deref(),
        Some("failure retry backoff"),
        "同 digest 失败后 backoff 内必须抑制重试"
    );
}

#[tokio::test]
async fn concurrent_extractions_serialize_via_session_gate() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::new(MockModel::memories_json(vec![json!({
        "title": "T", "content": "concurrent fact", "scope": "project", "type": "project"
    })])));
    let svc = extraction_service(stores.clone(), Arc::clone(&model)).await;
    let msgs = messages("user", "assistant");
    let (r1, r2) = tokio::join!(
        svc.extract_durable(msgs.clone(), ExtractionReason::FinalTurn),
        svc.extract_durable(msgs.clone(), ExtractionReason::FinalTurn),
    );
    let o1 = r1.unwrap();
    let o2 = r2.unwrap();
    // gate 串行：一个执行、一个因 digest 已成功而 skip
    let executed = o1.skipped.is_none() || o2.skipped.is_none();
    let duplicated = o1.skipped.as_deref() == Some("duplicate transcript digest")
        || o2.skipped.as_deref() == Some("duplicate transcript digest");
    assert!(
        executed && duplicated,
        "per-session gate 必须串行化并发抽取"
    );
    let hits = svc.search("concurrent fact", 10).await.unwrap();
    assert_eq!(hits.len(), 1, "并发抽取只应写入一条");
}

#[tokio::test]
async fn separate_restored_services_share_the_session_extraction_gate() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(CountingSlowExtractionModel {
        extraction_calls: AtomicUsize::new(0),
    });
    let context = context("proj-a", "restored-session");
    let svc_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context.clone(),
    )
    .await;
    let svc_b = service(stores, Arc::clone(&model) as Arc<dyn ModelClient>, context).await;
    let msgs = messages("user", "assistant");

    let (first, second) = tokio::join!(
        svc_a.extract_durable(msgs.clone(), ExtractionReason::FinalTurn),
        svc_b.extract_durable(msgs, ExtractionReason::FinalTurn),
    );

    let first = first.unwrap();
    let second = second.unwrap();
    assert!(
        first.skipped.is_none() || second.skipped.is_none(),
        "one restored service must perform the extraction"
    );
    assert!(
        first.skipped.as_deref() == Some("duplicate transcript digest")
            || second.skipped.as_deref() == Some("duplicate transcript digest"),
        "the other restored service must observe the persisted digest"
    );
    assert_eq!(
        model.extraction_calls.load(Ordering::SeqCst),
        1,
        "same persisted session must make only one extraction model request"
    );
}

#[tokio::test]
async fn set_session_id_rebinds_the_shared_extraction_gate() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(CountingSlowExtractionModel {
        extraction_calls: AtomicUsize::new(0),
    });
    let svc_a = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "pending-a"),
    )
    .await;
    let svc_b = service(
        stores,
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "pending-b"),
    )
    .await;
    svc_a.set_session_id("stable-session");
    svc_b.set_session_id("stable-session");
    let msgs = messages("user", "assistant");

    let (first, second) = tokio::join!(
        svc_a.extract_durable(msgs.clone(), ExtractionReason::FinalTurn),
        svc_b.extract_durable(msgs, ExtractionReason::FinalTurn),
    );

    let first = first.unwrap();
    let second = second.unwrap();
    assert!(first.skipped.is_none() || second.skipped.is_none());
    assert!(
        first.skipped.as_deref() == Some("duplicate transcript digest")
            || second.skipped.as_deref() == Some("duplicate transcript digest")
    );
    assert_eq!(model.extraction_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refresh_recomputes_ttl_expiry() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;

    let mut entry = record("refreshable body", MemoryScope::Private);
    entry.ttl_days = 1;
    svc.write_memory(entry).await.unwrap();
    // 把 expires_at 拨到过去（模拟已过期）
    let mut raw = None;
    let mut key = String::new();
    for k in stores.memories.list_prefix("personal/").await.unwrap() {
        if let Some(value) = stores.memories.get(&k).await.unwrap() {
            raw = Some(value);
            key = k;
        }
    }
    let value = raw.expect("row exists");
    let mut meta: DurableMemoryMetadata =
        serde_json::from_value(value.get("metadata").unwrap().clone()).unwrap();
    meta.expires_at_ms = Some(0);
    let mut entry_json = value.clone();
    entry_json["metadata"] = serde_json::to_value(&meta).unwrap();
    stores.memories.set(&key, &entry_json, None).await.unwrap();

    // 同 body 重写（refresh）：metadata 以新写入为准，TTL 重新计算
    let mut again = record("refreshable body", MemoryScope::Private);
    again.ttl_days = 1;
    svc.write_memory(again).await.unwrap();

    let hits = svc.search("refreshable body", 10).await.unwrap();
    assert_eq!(hits.len(), 1, "refresh 后 TTL 必须重新计算（不再过期）");
}

#[tokio::test]
async fn profile_mismatch_fails_closed() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc1 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    svc1.write_memory(record("seed body", MemoryScope::Private))
        .await
        .unwrap();
    drop(svc1);

    // 新 service 使用不同 embedding_profile → contract 不匹配
    let mut cfg = config();
    cfg.embedding_profile = "other-profile-v2".to_string();
    let svc2 = MemoryService::new(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
        cfg,
    )
    .await
    .unwrap();
    assert!(
        svc2.last_error().is_some(),
        "profile mismatch 必须在装配期记录 last_error"
    );
    let status_key = format!(
        "memory/status/{}/proj-a/s1/memory_last_error",
        owner_key_for_id("local")
    );
    let status = stores.kv.get(&status_key).await.unwrap();
    assert!(
        status
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .is_some_and(|message| message.contains("embedding contract profile mismatch")),
        "profile mismatch must be persisted under the scoped diagnostics key: {status:?}"
    );
    let err = svc2.search("seed body", 10).await;
    assert!(err.is_err(), "profile mismatch 的 recall 必须 fail closed");
    let err = svc2
        .write_memory(record("new body", MemoryScope::Private))
        .await;
    assert!(err.is_err(), "profile mismatch 的写入必须 fail closed");
}

#[tokio::test]
async fn project_documents_fail_closed_on_profile_mismatch() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "New-profile document content.").unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());

    // 先建立旧 profile 的共享 contract。
    let svc1 = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s1"),
    )
    .await;
    svc1.write_memory(record("seed old profile", MemoryScope::Private))
        .await
        .unwrap();
    drop(svc1);

    // P3 的 document 入口必须与 Personal recall/write 同样校验 profile；
    // 即使两种模型恰好输出相同维度，也不得把新空间向量写进旧 Document SoT。
    let mut cfg = p3_config();
    cfg.embedding_profile = "other-profile-v2".to_string();
    let svc2 = MemoryService::new(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s2"),
        cfg,
    )
    .await
    .unwrap();
    assert!(svc2.index_project_docs(&cwd).await.is_err());
    assert!(
        svc2.search_project_docs("document content", 5)
            .await
            .is_err()
    );
    assert!(
        stores
            .documents
            .list_prefix("project_doc/proj/")
            .await
            .unwrap()
            .is_empty(),
        "profile 不匹配时不得创建 project-document membership"
    );
}

// ── §20.9 P3 Documents ───────────────────────────────────────────

fn p3_config() -> MemoryServiceConfig {
    MemoryServiceConfig {
        index_project_docs: true,
        min_recall_score: 0.0,
        ..MemoryServiceConfig::default()
    }
}

async fn p3_service(
    stores: MemoryStores,
    model: Arc<dyn ModelClient>,
    ctx: MemoryContext,
) -> MemoryService {
    MemoryService::new(stores, model, ctx, p3_config())
        .await
        .expect("memory service")
}

#[tokio::test]
async fn project_doc_membership_isolates_projects() {
    let dir = tempfile::TempDir::new().unwrap();
    // 两个项目工作区各放一份 AGENTS.md（内容不同）
    let cwd_a = dir.path().join("proj-a");
    std::fs::create_dir_all(&cwd_a).unwrap();
    std::fs::write(
        cwd_a.join("AGENTS.md"),
        "Project A build system uses cargo workspace.",
    )
    .unwrap();
    let cwd_b = dir.path().join("proj-b");
    std::fs::create_dir_all(&cwd_b).unwrap();
    std::fs::write(
        cwd_b.join("AGENTS.md"),
        "Project B uses sqlx for database access.",
    )
    .unwrap();

    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc_a = p3_service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let svc_b = p3_service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-b", "s1"),
    )
    .await;

    assert_eq!(svc_a.index_project_docs(&cwd_a).await.unwrap(), 1);
    assert_eq!(svc_b.index_project_docs(&cwd_b).await.unwrap(), 1);

    // Project A 搜索仅返回 A 的文档
    let hits_a = svc_a
        .search_project_docs("cargo workspace", 5)
        .await
        .unwrap();
    assert_eq!(hits_a.len(), 1);
    assert!(hits_a[0].chunk.content.contains("cargo workspace"));
    // Project B 搜索仅返回 B 的文档
    let hits_b = svc_b.search_project_docs("sqlx database", 5).await.unwrap();
    assert_eq!(hits_b.len(), 1);
    assert!(hits_b[0].chunk.content.contains("sqlx"));
    // 交叉搜索：A 搜 B 的内容 → 不得返回 B 的文档（doc_ids 过滤生效，
    // 禁止无过滤全库搜索；A 自身文档可能低分命中，只断言不含 B 内容）
    let cross = svc_a.search_project_docs("sqlx database", 5).await.unwrap();
    assert!(
        !cross.iter().any(|h| h.chunk.content.contains("sqlx")),
        "Project A 不得返回 Project B 的文档"
    );
    // membership 键按 project scoped
    let keys_a = stores
        .documents
        .list_prefix("project_doc/proj-a/")
        .await
        .unwrap();
    let keys_b = stores
        .documents
        .list_prefix("project_doc/proj-b/")
        .await
        .unwrap();
    assert_eq!(keys_a.len(), 1);
    assert_eq!(keys_b.len(), 1);
}

#[tokio::test]
async fn same_source_hash_doc_reused_across_projects() {
    let dir = tempfile::TempDir::new().unwrap();
    // 两个项目放相同内容的 AGENTS.md（source_hash 相同）
    let cwd_a = dir.path().join("proj-a");
    std::fs::create_dir_all(&cwd_a).unwrap();
    std::fs::write(
        cwd_a.join("AGENTS.md"),
        "Shared instructions for all projects.",
    )
    .unwrap();
    let cwd_b = dir.path().join("proj-b");
    std::fs::create_dir_all(&cwd_b).unwrap();
    std::fs::write(
        cwd_b.join("AGENTS.md"),
        "Shared instructions for all projects.",
    )
    .unwrap();

    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc_a = p3_service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    let svc_b = p3_service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-b", "s1"),
    )
    .await;
    svc_a.index_project_docs(&cwd_a).await.unwrap();
    svc_b.index_project_docs(&cwd_b).await.unwrap();

    // source_hash 相同 → 复用同一 doc_id；不同项目各有 membership mapping
    let keys_a = stores
        .documents
        .list_prefix("project_doc/proj-a/")
        .await
        .unwrap();
    let keys_b = stores
        .documents
        .list_prefix("project_doc/proj-b/")
        .await
        .unwrap();
    assert_eq!(keys_a.len(), 1);
    assert_eq!(keys_b.len(), 1);
    let doc_a = keys_a[0].rsplit('/').next().unwrap().to_string();
    let doc_b = keys_b[0].rsplit('/').next().unwrap().to_string();
    assert_eq!(doc_a, doc_b, "同内容文档因 source_hash 复用同一 doc_id");
    // 两个项目都能搜索到（各走自己的 membership）
    let hits_a = svc_a
        .search_project_docs("shared instructions", 5)
        .await
        .unwrap();
    let hits_b = svc_b
        .search_project_docs("shared instructions", 5)
        .await
        .unwrap();
    assert_eq!(hits_a.len(), 1);
    assert_eq!(hits_b.len(), 1);
}

#[tokio::test]
async fn concurrent_project_document_indexing_reuses_one_document() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("AGENTS.md"),
        "Concurrent indexing must retain exactly one document source.",
    )
    .unwrap();

    let stores = open_stores(&dir);
    let model = Arc::new(SlowEmbedModel::new());
    let svc_a = p3_service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s1"),
    )
    .await;
    let svc_b = p3_service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s2"),
    )
    .await;

    let (first, second) = tokio::join!(
        svc_a.index_project_docs(&cwd),
        svc_b.index_project_docs(&cwd),
    );
    assert_eq!(first.unwrap(), 1);
    assert_eq!(second.unwrap(), 1);
    assert_eq!(
        model.embed_calls.load(Ordering::SeqCst),
        2,
        "one contract probe plus one document chunk embedding"
    );
    assert_eq!(stores.documents.list_prefix("doc/").await.unwrap().len(), 1);
    assert_eq!(
        stores
            .documents
            .list_prefix("project_doc/proj/")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn project_doc_index_disabled_by_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "content").unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s1"),
    )
    .await;
    let n = svc.index_project_docs(&cwd).await.unwrap();
    assert_eq!(n, 0, "index_project_docs 默认关闭（P3）");
}

#[tokio::test]
async fn enabled_project_docs_are_injected_into_dynamic_prompt() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("AGENTS.md"),
        "Project instructions require cargo workspace validation.",
    )
    .unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = p3_service(
        stores,
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s1"),
    )
    .await;
    assert_eq!(svc.index_project_docs(&cwd).await.unwrap(), 1);

    let prompt = svc.memory_prompt("cargo workspace", 5).await;
    assert!(prompt.contains("# Relevant Project Documents"));
    assert!(prompt.contains("cargo workspace validation"));

    // `top_k = 0` 是禁用本轮注入的合法配置，必须同步返回，不能在带
    // membership filter 的渐进过采样循环中卡住。
    assert!(
        svc.search_project_docs("cargo workspace", 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(svc.memory_prompt("cargo workspace", 0).await.is_empty());
}

#[tokio::test]
async fn project_document_reindex_replaces_stale_membership() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let path = cwd.join("AGENTS.md");
    std::fs::write(&path, "Old instruction: use legacy build command.").unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = p3_service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s1"),
    )
    .await;
    svc.index_project_docs(&cwd).await.unwrap();

    std::fs::write(&path, "Current instruction: use cargo test workspace.").unwrap();
    svc.index_project_docs(&cwd).await.unwrap();
    let hits = svc
        .search_project_docs("legacy build command cargo test", 10)
        .await
        .unwrap();
    assert!(
        !hits
            .iter()
            .any(|hit| hit.chunk.content.contains("legacy build command")),
        "内容更新后旧 document membership 不得继续参与当前项目召回"
    );
    assert!(
        hits.iter()
            .any(|hit| hit.chunk.content.contains("cargo test workspace"))
    );
    assert_eq!(
        stores
            .documents
            .list_prefix("project_doc/proj/")
            .await
            .unwrap()
            .len(),
        1,
        "同一 source 只能保留一个当前 doc membership"
    );
}

#[tokio::test]
async fn project_doc_shared_content_keeps_membership_when_one_source_changes_or_is_removed() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let shared = "Shared instruction: run cargo test before submitting.";
    std::fs::write(cwd.join("AGENTS.md"), shared).unwrap();
    let claude = cwd.join("CLAUDE.md");
    std::fs::write(&claude, shared).unwrap();

    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let svc = p3_service(
        stores.clone(),
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s1"),
    )
    .await;
    svc.index_project_docs(&cwd).await.unwrap();
    assert_eq!(
        stores
            .documents
            .list_prefix("project_doc/proj/")
            .await
            .unwrap()
            .len(),
        1,
        "同内容的两个 source 应复用同一个 membership"
    );

    // CLAUDE 改为新内容后，AGENTS 仍引用旧 doc_id；旧 membership 不能被
    // CLAUDE 的重索引撤销。
    std::fs::write(&claude, "Claude-only instruction: use cargo fmt.").unwrap();
    svc.index_project_docs(&cwd).await.unwrap();
    let after_change = svc
        .search_project_docs("cargo test cargo fmt", 10)
        .await
        .unwrap();
    assert!(after_change.iter().any(|hit| hit.chunk.content == shared));
    assert!(
        after_change
            .iter()
            .any(|hit| hit.chunk.content.contains("cargo fmt"))
    );

    // 删除 CLAUDE 后应撤销它的新 membership 并递增 revision，但 AGENTS 的
    // 共享内容仍可检索。
    let revision_before_delete = svc.revision();
    std::fs::remove_file(&claude).unwrap();
    svc.index_project_docs(&cwd).await.unwrap();
    assert!(svc.revision() > revision_before_delete);
    let after_delete = svc
        .search_project_docs("cargo test cargo fmt", 10)
        .await
        .unwrap();
    assert!(after_delete.iter().any(|hit| hit.chunk.content == shared));
    assert!(
        !after_delete
            .iter()
            .any(|hit| hit.chunk.content.contains("cargo fmt")),
        "被删除 source 的 document membership 不得残留"
    );
    assert_eq!(
        stores
            .documents
            .list_prefix("project_doc/proj/")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn config_top_k_inject_reaches_service() {
    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let model = Arc::new(MockModel::empty());
    let cfg = MemoryServiceConfig {
        top_k_inject: 2,
        ..MemoryServiceConfig::default()
    };
    let svc = MemoryService::new(
        stores,
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj", "s1"),
        cfg,
    )
    .await
    .unwrap();
    assert_eq!(svc.top_k_inject(), 2, "top_k_inject 配置必须生效（§18）");
}

#[tokio::test]
async fn persist_extraction_state_failure_is_observable() {
    let dir = tempfile::TempDir::new().unwrap();
    let backend = RedbBackend::open(dir.path().join("ains.redb")).expect("open redb");
    // 只让 kv 表第 2 次 set 失败（0 基索引 1）：第 1 次是 write_memory
    // 内的 embedding contract 写入，第 2 次是 extraction 状态持久化
    // （last_success_digest）。
    let failing_kv: Arc<dyn KvStore> =
        Arc::new(FailingKv::failing_at(backend.table(TABLE_KV), vec![1]));
    let stores = MemoryStores::from_parts(
        Arc::clone(&failing_kv),
        Arc::new(backend.table(TABLE_MEMORIES)),
        Arc::new(backend.table(TABLE_EMBEDDINGS)),
        Arc::new(backend.table(TABLE_DOCUMENTS)),
        Arc::new(backend.table(TABLE_HNSW_CACHE)),
    );
    let model = Arc::new(MockModel::new(MockModel::memories_json(vec![json!({
        "title": "T", "content": "observable fact", "scope": "project", "type": "project"
    })])));
    let svc = MemoryService::new(
        stores,
        Arc::clone(&model) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
        config(),
    )
    .await
    .unwrap();

    let msgs = messages("user prompt", "assistant answer");
    let outcome = svc
        .extract_durable(msgs.clone(), ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert_eq!(outcome.saved.len(), 1);
    // M1' 修复：persist 失败不再静默吞掉，必须可观测
    let err = svc.last_error().unwrap_or_default();
    assert!(
        err.contains("persist extraction state failed"),
        "persist 失败必须可观测: {err}"
    );
    // 幂等降级：success digest 未落盘 → 同 transcript 再次触发重新抽取
    let second = svc
        .extract_durable(msgs, ExtractionReason::FinalTurn)
        .await
        .unwrap();
    assert_ne!(
        second.skipped.as_deref(),
        Some("duplicate transcript digest"),
        "digest 未持久化时不得误判重复"
    );
}

#[tokio::test]
async fn extraction_failure_persists_scoped_error_status() {
    struct EmbedFailModel;

    #[async_trait::async_trait]
    impl ModelClient for EmbedFailModel {
        async fn stream_response(
            &self,
            _request: ModelRequest,
        ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
            let message = ConversationMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: MockModel::memories_json(vec![json!({
                        "title": "T", "content": "fact", "scope": "project", "type": "project"
                    })]),
                }],
            };
            Ok(futures::stream::iter(vec![ModelStreamEvent::Complete {
                message,
                usage: UsageSnapshot::default(),
                stop_reason: None,
            }])
            .boxed())
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, AgentError> {
            Err(AgentError::Model("embedding unavailable".into()))
        }

        async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
            Err(AgentError::Model("stt unsupported".into()))
        }

        async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
            Err(AgentError::Model("tts unsupported".into()))
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    let stores = open_stores(&dir);
    let svc = service(
        stores.clone(),
        Arc::new(EmbedFailModel) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    assert!(
        svc.extract_durable(messages("user", "assistant"), ExtractionReason::FinalTurn)
            .await
            .is_err()
    );
    let error = stores
        .kv
        .get(&format!(
            "memory/status/{}/proj-a/s1/extract_last_error",
            owner_key_for_id("local")
        ))
        .await
        .unwrap()
        .and_then(|value| value.as_str().map(str::to_owned));
    assert!(
        error
            .as_ref()
            .is_some_and(|message| message.contains("embedding failed")),
        "extraction 错误必须按 project/session 持久化: {error:?}"
    );
}

#[tokio::test]
async fn manifest_skips_encryption_error_rows() {
    let dir = tempfile::TempDir::new().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("ains.redb")).expect("open redb"));
    // 用 key A 写一条 durable memory
    let key_a = EncryptionKey::from_bytes([7u8; 32]);
    let stores_a = open_memory_stores(&MemoryBackend::Native(Arc::clone(&backend)), Some(key_a));
    let svc = service(
        stores_a,
        Arc::new(MockModel::empty()) as Arc<dyn ModelClient>,
        context("proj-a", "s1"),
    )
    .await;
    svc.write_memory(record("encrypted fact", MemoryScope::Private))
        .await
        .unwrap();
    // 用 key B 打开同一 backend：单行密文无法解 → Encryption 错误
    let key_b = EncryptionKey::from_bytes([9u8; 32]);
    let stores_b = open_memory_stores(&MemoryBackend::Native(Arc::clone(&backend)), Some(key_b));
    let manifest =
        build_durable_manifest(&*stores_b.memories, &context("proj-a", "manifest")).await;
    // M2' 修复：单行加密错误跳过，不中止整个清单
    assert!(
        manifest.is_ok(),
        "单行加密错误不得中止清单: {:?}",
        manifest.err()
    );
    assert!(manifest.unwrap().is_empty(), "加密行被跳过不进入清单");
}
