//! Phase 0 核心 trait 行为契约测试（Native）：以内存 mock 验证各 trait
//! 可被实现、可通过 `dyn` 使用，并覆盖 namespace 隔离等关键行为。

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};

use agent_core::error::{AgentError, MemoryError, ToolError};
use agent_core::kernel::messages::{ContentBlock, ConversationMessage, Role};
use agent_core::memory::kv::KvStore;
use agent_core::memory::vector::{
    MemoryNamespace, Metric, VectorIndex, VectorIndexConfig, VectorIndexManager,
};
use agent_core::model_client::{
    DEFAULT_MAX_OUTPUT_TOKENS, EventStream, ModelClient, ModelRequest, ModelStreamEvent,
    UsageSnapshot,
};
use agent_core::platform::Platform;
use agent_core::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolMetadata, ToolResult};

// ── KvStore mock ────────────────────────────────────────────────

#[derive(Default)]
struct MemKvStore {
    map: Mutex<HashMap<String, Value>>,
}

#[async_trait::async_trait]
impl KvStore for MemKvStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
        Ok(self.map.lock().unwrap().get(key).cloned())
    }

    async fn set(
        &self,
        key: &str,
        value: &Value,
        _ttl: Option<Duration>,
    ) -> Result<(), MemoryError> {
        self.map
            .lock()
            .unwrap()
            .insert(key.to_string(), value.clone());
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), MemoryError> {
        self.map.lock().unwrap().remove(key);
        Ok(())
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
        let mut keys: Vec<String> = self
            .map
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        keys.sort();
        Ok(keys)
    }
}

#[tokio::test]
async fn kv_store_contract_via_dyn() {
    let store = MemKvStore::default();
    let kv: &dyn KvStore = &store;

    assert_eq!(kv.get("a").await.unwrap(), None);
    kv.set("a:1", &json!({"v": 1}), None).await.unwrap();
    kv.set("a:2", &json!({"v": 2}), Some(Duration::from_secs(60)))
        .await
        .unwrap();
    kv.set("b:1", &json!(true), None).await.unwrap();

    assert_eq!(kv.get("a:1").await.unwrap(), Some(json!({"v": 1})));
    assert_eq!(
        kv.list_prefix("a:").await.unwrap(),
        vec!["a:1".to_string(), "a:2".to_string()]
    );

    kv.delete("a:1").await.unwrap();
    assert_eq!(kv.get("a:1").await.unwrap(), None);
    assert_eq!(kv.list_prefix("a:").await.unwrap(), vec!["a:2".to_string()]);
}

// ── Tool mock ───────────────────────────────────────────────────

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "echo".into(),
            description: "echo the input text".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let text = input["text"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing `text`".into()))?;
        ctx.metadata
            .extra
            .insert("echo_calls".into(), json!(ctx.metadata.extra.len() + 1));
        Ok(ToolResult::ok(text))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

#[tokio::test]
async fn tool_execute_writes_metadata_and_returns_output() {
    let tool: Box<dyn Tool> = Box::new(EchoTool);
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/tmp"),
        metadata: &mut metadata,
    };

    let result = tool.execute(json!({"text": "hi"}), &mut ctx).await.unwrap();
    assert_eq!(result.output, "hi");
    assert!(!result.is_error);
    assert_eq!(metadata.extra.get("echo_calls"), Some(&json!(1)));
    assert!(tool.is_read_only(&json!({"text": "hi"})));
}

#[tokio::test]
async fn tool_invalid_input_maps_to_tool_error_and_agent_error() {
    let tool = EchoTool;
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/tmp"),
        metadata: &mut metadata,
    };

    let err = tool.execute(json!({}), &mut ctx).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)));
    let agent_err: AgentError = err.into();
    assert!(matches!(agent_err, AgentError::Tool(_)));
}

#[test]
fn tool_result_constructors() {
    let ok = ToolResult::ok("done");
    assert!(!ok.is_error);
    assert_eq!(ok.output, "done");
    assert_eq!(ok.metadata, Value::Null);

    let err = ToolResult::err("boom");
    assert!(err.is_error);
    assert_eq!(err.output, "boom");
}

// ── ModelClient mock ────────────────────────────────────────────

struct ScriptedModelClient;

#[async_trait::async_trait]
impl ModelClient for ScriptedModelClient {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
        let complete = ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "hello world".into(),
            }],
        };
        let events = vec![
            ModelStreamEvent::TextDelta {
                text: "hello ".into(),
            },
            ModelStreamEvent::Retry {
                message: "overloaded".into(),
                attempt: 1,
                max_attempts: 3,
                delay_secs: 0.5,
            },
            ModelStreamEvent::TextDelta {
                text: "world".into(),
            },
            ModelStreamEvent::Complete {
                message: complete,
                usage: UsageSnapshot {
                    input_tokens: 10,
                    output_tokens: 2,
                },
                stop_reason: Some("end_turn".into()),
            },
        ];
        Ok(futures::stream::iter(events).boxed())
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AgentError> {
        Ok(vec![text.len() as f32, 0.0])
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, AgentError> {
        Ok("transcript".into())
    }

    async fn tts(&self, text: &str) -> Result<Vec<u8>, AgentError> {
        Ok(text.as_bytes().to_vec())
    }
}

#[tokio::test]
async fn model_client_stream_protocol_delta_retry_complete() {
    let client: &dyn ModelClient = &ScriptedModelClient;
    let mut stream = client
        .stream_response(ModelRequest::default())
        .await
        .unwrap();

    let mut text = String::new();
    let mut retries = 0u32;
    let mut completed = false;
    while let Some(event) = stream.next().await {
        match event {
            ModelStreamEvent::TextDelta { text: t } => text.push_str(&t),
            ModelStreamEvent::Retry { attempt, .. } => {
                retries += 1;
                assert_eq!(attempt, 1);
            }
            ModelStreamEvent::Complete {
                message,
                usage,
                stop_reason,
            } => {
                completed = true;
                assert_eq!(message.role, Role::Assistant);
                assert_eq!(
                    usage,
                    UsageSnapshot {
                        input_tokens: 10,
                        output_tokens: 2
                    }
                );
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
            }
        }
    }
    assert_eq!(text, "hello world");
    assert_eq!(retries, 1);
    assert!(completed);

    assert_eq!(client.embed("abc").await.unwrap(), vec![3.0, 0.0]);
    assert_eq!(client.stt(&[0u8]).await.unwrap(), "transcript");
    assert_eq!(client.tts("x").await.unwrap(), b"x".to_vec());
}

#[test]
fn model_request_default_max_output_tokens_is_baseline_4096() {
    let request = ModelRequest::default();
    assert_eq!(request.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    assert_eq!(request.max_output_tokens, 4096);
    assert!(request.model.is_none());
    assert!(request.messages.is_empty());
    assert!(request.tools.is_empty());
}

// ── VectorIndex / VectorIndexManager mock ───────────────────────

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[derive(Default)]
struct LinearVectorIndex {
    entries: Vec<(String, Vec<f32>)>,
}

#[async_trait::async_trait]
impl VectorIndex for LinearVectorIndex {
    async fn add(&mut self, node_id: &str, vector: &[f32]) -> Result<(), MemoryError> {
        self.entries.push((node_id.to_string(), vector.to_vec()));
        Ok(())
    }

    async fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<(String, f32)>, MemoryError> {
        let mut scored: Vec<(String, f32)> = self
            .entries
            .iter()
            .map(|(id, v)| (id.clone(), cosine(query, v)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(top_k);
        Ok(scored)
    }

    async fn remove(&mut self, node_id: &str) -> Result<(), MemoryError> {
        let before = self.entries.len();
        self.entries.retain(|(id, _)| id != node_id);
        if self.entries.len() == before {
            return Err(MemoryError::NotFound(node_id.to_string()));
        }
        Ok(())
    }

    async fn save(&self, kv: &dyn KvStore) -> Result<(), MemoryError> {
        let ids: Vec<&str> = self.entries.iter().map(|(id, _)| id.as_str()).collect();
        kv.set("index:node_ids", &json!(ids), None).await
    }
}

#[derive(Default)]
struct MapIndexManager {
    indexes: HashMap<MemoryNamespace, LinearVectorIndex>,
}

#[async_trait::async_trait]
impl VectorIndexManager for MapIndexManager {
    async fn get_index(&self, namespace: MemoryNamespace) -> Result<&dyn VectorIndex, MemoryError> {
        self.indexes
            .get(&namespace)
            .map(|index| index as &dyn VectorIndex)
            .ok_or(MemoryError::NamespaceNotFound(namespace))
    }

    async fn get_index_mut(
        &mut self,
        namespace: MemoryNamespace,
    ) -> Result<&mut dyn VectorIndex, MemoryError> {
        self.indexes
            .get_mut(&namespace)
            .map(|index| index as &mut dyn VectorIndex)
            .ok_or(MemoryError::NamespaceNotFound(namespace))
    }

    async fn create_index(
        &mut self,
        namespace: MemoryNamespace,
        _config: VectorIndexConfig,
    ) -> Result<(), MemoryError> {
        self.indexes.insert(namespace, LinearVectorIndex::default());
        Ok(())
    }

    async fn remove_index(&mut self, namespace: MemoryNamespace) -> Result<(), MemoryError> {
        // 契约要求幂等（ensure-absent）
        self.indexes.remove(&namespace);
        Ok(())
    }
}

fn test_config() -> VectorIndexConfig {
    VectorIndexConfig {
        dimension: 2,
        distance_metric: Metric::Cosine,
        m: 16,
        ef: 50,
    }
}

#[tokio::test]
async fn vector_index_search_ranks_by_similarity() {
    let mut index = LinearVectorIndex::default();
    index.add("x", &[1.0, 0.0]).await.unwrap();
    index.add("y", &[0.0, 1.0]).await.unwrap();
    index.add("z", &[0.7, 0.7]).await.unwrap();

    let hits = index.search(&[1.0, 0.0], 2).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0, "x");
    assert_eq!(hits[1].0, "z");
    assert!(hits[0].1 > hits[1].1);

    index.remove("x").await.unwrap();
    let hits = index.search(&[1.0, 0.0], 10).await.unwrap();
    assert!(hits.iter().all(|(id, _)| id != "x"));
    assert!(matches!(
        index.remove("x").await.unwrap_err(),
        MemoryError::NotFound(_)
    ));
}

#[tokio::test]
async fn vector_index_manager_namespaces_are_isolated() {
    let mut manager = MapIndexManager::default();
    manager
        .create_index(MemoryNamespace::Personal, test_config())
        .await
        .unwrap();
    manager
        .create_index(MemoryNamespace::Document, test_config())
        .await
        .unwrap();

    manager
        .get_index_mut(MemoryNamespace::Personal)
        .await
        .unwrap()
        .add("p1", &[1.0, 0.0])
        .await
        .unwrap();
    manager
        .get_index_mut(MemoryNamespace::Document)
        .await
        .unwrap()
        .add("d1", &[1.0, 0.0])
        .await
        .unwrap();

    let personal_hits = manager
        .get_index(MemoryNamespace::Personal)
        .await
        .unwrap()
        .search(&[1.0, 0.0], 10)
        .await
        .unwrap();
    assert_eq!(personal_hits.len(), 1);
    assert_eq!(personal_hits[0].0, "p1");

    let err = manager
        .get_index(MemoryNamespace::Code)
        .await
        .err()
        .expect("Code namespace should not exist");
    assert!(matches!(
        err,
        MemoryError::NamespaceNotFound(MemoryNamespace::Code)
    ));

    manager
        .remove_index(MemoryNamespace::Document)
        .await
        .unwrap();
    assert!(manager.get_index(MemoryNamespace::Document).await.is_err());
}

#[tokio::test]
async fn vector_index_save_persists_derived_state_to_kv() {
    let mut index = LinearVectorIndex::default();
    index.add("n1", &[1.0, 0.0]).await.unwrap();
    index.add("n2", &[0.0, 1.0]).await.unwrap();

    let kv = MemKvStore::default();
    index.save(&kv).await.unwrap();
    assert_eq!(
        kv.get("index:node_ids").await.unwrap(),
        Some(json!(["n1", "n2"]))
    );
}

// ── 序列化契约 ──────────────────────────────────────────────────

#[test]
fn enum_wire_format_is_snake_case() {
    assert_eq!(
        serde_json::to_value(MemoryNamespace::EnterpriseKnowledge).unwrap(),
        json!("enterprise_knowledge")
    );
    assert_eq!(
        serde_json::to_value(Metric::Cosine).unwrap(),
        json!("cosine")
    );
    assert_eq!(
        serde_json::to_value(ToolCategory::FileSystem).unwrap(),
        json!("file_system")
    );
    assert_eq!(
        serde_json::from_value::<MemoryNamespace>(json!("personal")).unwrap(),
        MemoryNamespace::Personal
    );
}

#[test]
fn unknown_content_block_type_is_rejected() {
    let result = serde_json::from_value::<ContentBlock>(json!({
        "type": "thinking",
        "text": "…",
    }));
    assert!(result.is_err());
}

#[test]
fn image_and_tool_result_blocks_wire_shape() {
    let image = ContentBlock::Image {
        media_type: "image/png".into(),
        data: "aGk=".into(),
    };
    let value = serde_json::to_value(&image).unwrap();
    assert_eq!(
        value,
        json!({"type": "image", "media_type": "image/png", "data": "aGk="})
    );
    assert_eq!(
        serde_json::from_value::<ContentBlock>(value).unwrap(),
        image
    );

    let tool_result = ContentBlock::ToolResult {
        tool_use_id: "toolu_1".into(),
        content: "boom".into(),
        is_error: true,
        result_metadata: json!({"returncode": 1}),
    };
    let value = serde_json::to_value(&tool_result).unwrap();
    assert_eq!(value["type"], "tool_result");
    assert_eq!(value["is_error"], json!(true));
    assert_eq!(value["result_metadata"], json!({"returncode": 1}));
    assert_eq!(
        serde_json::from_value::<ContentBlock>(value).unwrap(),
        tool_result
    );
}

#[test]
fn model_request_wire_roundtrip_with_tools() {
    let request = ModelRequest {
        model: Some("ains-chat".into()),
        messages: vec![ConversationMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: Some("be brief".into()),
        max_output_tokens: 1024,
        tools: vec![ToolDef {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: json!({"type": "object"}),
        }],
    };
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["model"], "ains-chat");
    assert_eq!(value["max_output_tokens"], 1024);
    assert_eq!(value["tools"][0]["name"], "echo");
    assert_eq!(value["tools"][0]["input_schema"]["type"], "object");

    let back: ModelRequest = serde_json::from_value(value).unwrap();
    assert_eq!(back.model.as_deref(), Some("ains-chat"));
    assert_eq!(back.messages, request.messages);
    assert_eq!(back.max_output_tokens, 1024);
    assert_eq!(back.tools.len(), 1);
}

#[test]
fn agent_error_wraps_memory_and_skills_errors() {
    let memory: AgentError = MemoryError::NotFound("m1".into()).into();
    assert!(matches!(memory, AgentError::Memory(_)));

    let skills: AgentError = SkillsError::NotFound("s1".into()).into();
    assert!(matches!(skills, AgentError::Skills(_)));
    assert_eq!(skills.to_string(), "skill not found: s1");
}

#[test]
fn platform_current_is_not_web_on_native() {
    assert_ne!(Platform::current(), Platform::Web);
}

// ── DocumentStore mock ──────────────────────────────────────────

use agent_core::memory::document::{DocumentChunk, DocumentMeta, DocumentStore, SearchResult};

#[derive(Default)]
struct MemDocumentStore {
    docs: Vec<DocumentMeta>,
}

#[async_trait::async_trait]
impl DocumentStore for MemDocumentStore {
    async fn index(&mut self, file_path: &str) -> Result<DocumentMeta, MemoryError> {
        let meta = DocumentMeta {
            id: format!("doc-{}", self.docs.len() + 1),
            name: file_path.to_string(),
            chunk_count: 1,
            source_hash: format!("hash:{file_path}"),
        };
        self.docs.push(meta.clone());
        Ok(meta)
    }

    async fn search(
        &self,
        query: &str,
        top_k: usize,
        doc_ids: Option<&[String]>,
    ) -> Result<Vec<SearchResult>, MemoryError> {
        let mut hits: Vec<SearchResult> = self
            .docs
            .iter()
            .filter(|d| doc_ids.is_none_or(|ids| ids.contains(&d.id)))
            .filter(|d| d.name.contains(query))
            .map(|d| SearchResult {
                chunk: DocumentChunk {
                    chunk_id: format!("{}:0", d.id),
                    doc_id: d.id.clone(),
                    content: d.name.clone(),
                },
                doc_name: d.name.clone(),
                score: 1.0,
            })
            .collect();
        hits.truncate(top_k);
        Ok(hits)
    }

    async fn list_docs(&self) -> Result<Vec<DocumentMeta>, MemoryError> {
        Ok(self.docs.clone())
    }

    async fn delete(&mut self, doc_id: &str) -> Result<(), MemoryError> {
        let before = self.docs.len();
        self.docs.retain(|d| d.id != doc_id);
        if self.docs.len() == before {
            return Err(MemoryError::NotFound(doc_id.to_string()));
        }
        Ok(())
    }

    async fn is_indexed(&self, source_hash: &str) -> Result<bool, MemoryError> {
        Ok(self.docs.iter().any(|d| d.source_hash == source_hash))
    }
}

#[tokio::test]
async fn document_store_contract_index_dedup_scope_delete() {
    let mut store = MemDocumentStore::default();
    let meta = store.index("notes/alpha.md").await.unwrap();
    store.index("notes/beta.md").await.unwrap();

    // source_hash 去重判断
    assert!(store.is_indexed("hash:notes/alpha.md").await.unwrap());
    assert!(!store.is_indexed("hash:unknown").await.unwrap());

    // doc_ids 限定检索范围
    let all = store.search("notes", 10, None).await.unwrap();
    assert_eq!(all.len(), 2);
    let scoped = store
        .search("notes", 10, Some(std::slice::from_ref(&meta.id)))
        .await
        .unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].chunk.doc_id, meta.id);

    // top_k 截断
    assert_eq!(store.search("notes", 1, None).await.unwrap().len(), 1);

    store.delete(&meta.id).await.unwrap();
    assert_eq!(store.list_docs().await.unwrap().len(), 1);
    assert!(matches!(
        store.delete(&meta.id).await,
        Err(MemoryError::NotFound(_))
    ));
}

// ── SkillLoader / SkillManage mock ──────────────────────────────

use agent_core::error::SkillsError;
use agent_core::skills::{SkillContent, SkillContext, SkillLoader, SkillManage, SkillSummary};

/// 内存 skill 库：`list` 阶段按 `available_tools` 门控过滤（不匹配完全不可见）。
#[derive(Default)]
struct MemSkillStore {
    skills: Mutex<HashMap<String, (SkillSummary, String)>>,
}

impl MemSkillStore {
    fn summary(name: &str, requires_tools: Vec<String>) -> SkillSummary {
        SkillSummary {
            name: name.to_string(),
            description: format!("skill {name}"),
            category: "test".into(),
            requires_tools,
        }
    }
}

#[async_trait::async_trait]
impl SkillLoader for MemSkillStore {
    async fn list(&self, ctx: &SkillContext) -> Result<Vec<SkillSummary>, SkillsError> {
        let mut out: Vec<SkillSummary> = self
            .skills
            .lock()
            .unwrap()
            .values()
            .filter(|(s, _)| {
                s.requires_tools
                    .iter()
                    .all(|t| ctx.available_tools.contains(t))
            })
            .map(|(s, _)| s.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn load(&self, name: &str) -> Result<SkillContent, SkillsError> {
        let skills = self.skills.lock().unwrap();
        let (_, body) = skills
            .get(name)
            .ok_or_else(|| SkillsError::NotFound(name.to_string()))?;
        Ok(SkillContent {
            frontmatter: serde_yaml::from_str(&format!("name: {name}"))
                .map_err(|e| SkillsError::InvalidFormat(e.to_string()))?,
            body: body.clone(),
        })
    }

    async fn load_reference(&self, name: &str, path: &str) -> Result<String, SkillsError> {
        if !self.skills.lock().unwrap().contains_key(name) {
            return Err(SkillsError::NotFound(name.to_string()));
        }
        Ok(format!("reference {path} of {name}"))
    }
}

#[async_trait::async_trait]
impl SkillManage for MemSkillStore {
    async fn create_skill(&self, name: &str, content: &str) -> Result<SkillSummary, SkillsError> {
        let summary = Self::summary(name, vec![]);
        self.skills
            .lock()
            .unwrap()
            .insert(name.to_string(), (summary.clone(), content.to_string()));
        Ok(summary)
    }

    async fn update_skill(&self, name: &str, content: &str) -> Result<SkillSummary, SkillsError> {
        let mut skills = self.skills.lock().unwrap();
        let entry = skills
            .get_mut(name)
            .ok_or_else(|| SkillsError::NotFound(name.to_string()))?;
        entry.1 = content.to_string();
        Ok(entry.0.clone())
    }

    async fn rollback_skill(
        &self,
        name: &str,
        _target_version: &str,
    ) -> Result<SkillSummary, SkillsError> {
        self.skills
            .lock()
            .unwrap()
            .get(name)
            .map(|(s, _)| s.clone())
            .ok_or_else(|| SkillsError::NotFound(name.to_string()))
    }

    async fn delete_skill(&self, name: &str) -> Result<(), SkillsError> {
        self.skills
            .lock()
            .unwrap()
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| SkillsError::NotFound(name.to_string()))
    }
}

#[tokio::test]
async fn skill_list_gates_by_available_tools() {
    let store = MemSkillStore::default();
    store.create_skill("plain", "body-a").await.unwrap();
    store.skills.lock().unwrap().insert(
        "needs-shell".into(),
        (
            MemSkillStore::summary("needs-shell", vec!["shell".into()]),
            "body-b".into(),
        ),
    );

    let loader: &dyn SkillLoader = &store;
    let no_shell = SkillContext {
        platform: Platform::Desktop,
        available_tools: vec!["calculator".into()],
    };
    let names: Vec<String> = loader
        .list(&no_shell)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["plain".to_string()]);

    let with_shell = SkillContext {
        platform: Platform::Desktop,
        available_tools: vec!["shell".into()],
    };
    assert_eq!(loader.list(&with_shell).await.unwrap().len(), 2);
}

#[tokio::test]
async fn skill_manage_lifecycle_and_not_found_errors() {
    let store = MemSkillStore::default();
    let manage: &dyn SkillManage = &store;
    let loader: &dyn SkillLoader = &store;

    manage.create_skill("deploy", "v1").await.unwrap();
    assert_eq!(loader.load("deploy").await.unwrap().body, "v1");
    assert_eq!(
        loader.load("deploy").await.unwrap().frontmatter["name"],
        serde_yaml::Value::from("deploy")
    );

    manage.update_skill("deploy", "v2").await.unwrap();
    assert_eq!(loader.load("deploy").await.unwrap().body, "v2");
    assert!(
        loader
            .load_reference("deploy", "templates/x.md")
            .await
            .unwrap()
            .contains("templates/x.md")
    );

    manage.delete_skill("deploy").await.unwrap();
    assert!(matches!(
        loader.load("deploy").await,
        Err(SkillsError::NotFound(_))
    ));
    assert!(matches!(
        manage.update_skill("deploy", "v3").await,
        Err(SkillsError::NotFound(_))
    ));
}

// ── 编译期断言：对象安全 + Native 端 Send/Sync 边界 ─────────────

/// 七个核心 trait 必须保持对象安全（Phase 3 注册表 / Phase 2 存储层依赖 dyn
/// 分发；`dyn Trait` 作为类型出现即隐含对象安全证明）；同时 Native 端
/// `MaybeSendSync` 必须收敛为 `Send + Sync`（tokio 多线程调度前提，
/// supertrait 被误删时此处编译失败）。
#[test]
fn trait_objects_are_send_sync_on_native() {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send_sync::<dyn KvStore>();
    assert_send_sync::<dyn Tool>();
    assert_send_sync::<dyn VectorIndex>();
    assert_send_sync::<dyn VectorIndexManager>();
    assert_send_sync::<dyn DocumentStore>();
    assert_send_sync::<dyn SkillLoader>();
    assert_send_sync::<dyn SkillManage>();
    assert_send_sync::<dyn ModelClient>();
}
