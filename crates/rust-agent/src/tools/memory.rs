//! Memory 工具（P3，§19）：`memory_read` / `memory_write`。
//!
//! 直接操作生产 MemoryService（scoped recall / durable write），而非 memdir：
//! - `memory_read`：语义搜索当前 project/session 可见的 durable memory（只读）；
//! - `memory_write`：写入一条 durable memory（scope/ttl/tags 由调用方指定，
//!   Team scope 无 team context 时 fail closed）。
//!
//! 工具在装配期始终注册（注册集静态稳定，`REGISTERED_TOOL_NAMES` 缓存一致），
//! 会话装配完成 MemoryService 后再 `attach`；未 attach 时执行返回工具错误，
//! 不阻断主 Agent 对话路径。

use std::sync::Arc;
use std::sync::RwLock;

use serde_json::Value;

use crate::error::{MemoryError, ToolError};
use crate::marker::MaybeSendSync;
use crate::memory::memdir::{MemoryScope, MemoryType, NewMemoryEntry};
use crate::memory::service::MemoryService;
use crate::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

/// 单次 memory_read 最多返回条数。
const MEMORY_READ_MAX_RESULTS: usize = 20;

/// Durable-memory write boundary used by [`MemoryWriteTool`].  Hosts that
/// share storage across runtimes can wrap this operation in their process- or
/// origin-wide session lock before delegating to [`MemoryService`].
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait MemoryWriter: MaybeSendSync {
    async fn write(&self, record: NewMemoryEntry) -> Result<String, MemoryError>;
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl MemoryWriter for MemoryService {
    async fn write(&self, record: NewMemoryEntry) -> Result<String, MemoryError> {
        self.write_memory(record).await
    }
}

fn require_string(input: &Value, field: &str) -> Result<String, ToolError> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidInput(format!("missing required string field: {field}")))
}

fn require_query(input: &Value) -> Result<String, ToolError> {
    require_string(input, "query")
}

fn require_content(input: &Value) -> Result<String, ToolError> {
    require_string(input, "content")
}

/// 读取已 attach 的 MemoryService；未装配（disable / 初始化失败）时返回工具错误。
fn attached_service(
    service: &Arc<RwLock<Option<Arc<MemoryService>>>>,
) -> Result<Arc<MemoryService>, ToolError> {
    service
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or_else(|| ToolError::Execution("memory service unavailable".into()))
}

/// 语义搜索 durable memory（只读；scoped + TTL 过滤由 MemoryService 保证）。
#[derive(Clone)]
pub struct MemoryReadTool {
    service: Arc<RwLock<Option<Arc<MemoryService>>>>,
}

impl MemoryReadTool {
    /// wasm 单线程下 `Arc<RwLock<_>>` 非 Send/Sync 无害。
    #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
    pub fn new() -> Self {
        Self {
            service: Arc::new(RwLock::new(None)),
        }
    }

    /// 会话装配完成后注入 MemoryService（幂等；未注入时执行报错）。
    pub fn attach(&self, service: Arc<MemoryService>) {
        *self.service.write().unwrap_or_else(|p| p.into_inner()) = Some(service);
    }
}

impl Default for MemoryReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for MemoryReadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_read".into(),
            description: "Search durable memory scoped to the current project and session. \
                          Returns remembered facts relevant to the query (may be empty)."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Semantic search query"},
                    "top_k": {"type": "integer", "minimum": 1, "maximum": MEMORY_READ_MAX_RESULTS,
                              "description": "Maximum number of results (default 5)"}
                },
                "required": ["query"]
            }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let service = attached_service(&self.service)?;
        let query = require_query(&input)?;
        // 默认召回数走 MemoryServiceConfig.top_k_inject（§18），而非硬编码。
        let top_k = input
            .get("top_k")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or_else(|| service.top_k_inject())
            .clamp(1, MEMORY_READ_MAX_RESULTS);
        match service.search(&query, top_k).await {
            Ok(hits) if hits.is_empty() => Ok(ToolResult::ok("(no relevant memory)")),
            Ok(hits) => {
                let lines = hits
                    .iter()
                    .map(|hit| format!("- {}: {}", hit.title, hit.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolResult::ok(lines))
            }
            Err(e) => Ok(ToolResult::err(format!("memory search failed: {e}"))),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }
}

/// 写入一条 durable memory（写工具；Team scope 无 team context 时 fail closed）。
#[derive(Clone)]
pub struct MemoryWriteTool {
    writer: Arc<RwLock<Option<Arc<dyn MemoryWriter>>>>,
}

impl MemoryWriteTool {
    /// wasm 单线程下 `Arc<RwLock<_>>` 非 Send/Sync 无害。
    #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
    pub fn new() -> Self {
        Self {
            writer: Arc::new(RwLock::new(None)),
        }
    }

    /// 会话装配完成后注入 MemoryService（幂等；未注入时执行报错）。
    pub fn attach(&self, service: Arc<MemoryService>) {
        self.attach_writer(service);
    }

    /// 注入宿主提供的写入边界。Web 宿主使用它将 `memory_write` 与跨标签页
    /// 的会话清空操作串行化；Native 可继续使用 [`Self::attach`]。
    pub fn attach_writer(&self, writer: Arc<dyn MemoryWriter>) {
        *self.writer.write().unwrap_or_else(|p| p.into_inner()) = Some(writer);
    }
}

impl Default for MemoryWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for MemoryWriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_write".into(),
            description: "Write a durable memory entry. The entry is scoped to the current \
                          project by default; scope may be 'private' or 'project'. Saving is \
                          idempotent per (scope, content)."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {"type": "string", "description": "The durable fact to remember"},
                    "title": {"type": "string", "description": "Short title (defaults to content prefix)"},
                    "description": {"type": "string", "description": "One-line description (optional)"},
                    "memory_type": {"type": "string", "enum": ["user", "feedback", "project", "reference"],
                                    "description": "Entry type (default 'project')"},
                    "scope": {"type": "string", "enum": ["private", "project", "team"],
                              "description": "Visibility scope (default 'project')"},
                    "importance": {"type": "number", "minimum": 0.0,
                                   "description": "Importance weight (default 1.0)"},
                    "ttl_days": {"type": "integer", "minimum": 0,
                                 "description": "Expiry in days; 0 = never (default)"},
                    "tags": {"type": "array", "items": {"type": "string"},
                             "description": "Tags (optional)"}
                },
                "required": ["content"]
            }),
        }
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let writer = self
            .writer
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| ToolError::Execution("memory service unavailable".into()))?;
        let content = require_content(&input)?;
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| content.chars().take(60).collect());
        let record = NewMemoryEntry {
            title,
            body: content,
            description: input
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            memory_type: MemoryType::parse_lenient(
                input
                    .get("memory_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            scope: MemoryScope::parse_lenient(
                input
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("project"),
            ),
            importance: input
                .get("importance")
                .and_then(Value::as_f64)
                .unwrap_or(1.0)
                .max(0.0),
            source: "memory_write".to_string(),
            ttl_days: input
                .get("ttl_days")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0),
            tags: input
                .get("tags")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        };
        match writer.write(record).await {
            Ok(id) => Ok(ToolResult::ok(format!("saved memory {id}"))),
            Err(e) => Ok(ToolResult::err(format!("memory write failed: {e}"))),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::{MemoryWriteTool, MemoryWriter, NewMemoryEntry, require_content, require_query};
    use crate::error::MemoryError;
    use crate::tools::{Tool, ToolContext, ToolMetadata};

    #[test]
    fn memory_write_reads_documented_content_field() {
        let input = serde_json::json!({"content": " durable fact "});
        assert_eq!(require_content(&input).unwrap(), "durable fact");
        assert!(require_query(&input).is_err());
    }

    #[test]
    fn memory_read_still_requires_query_field() {
        let input = serde_json::json!({"query": " find fact "});
        assert_eq!(require_query(&input).unwrap(), "find fact");
        assert!(require_content(&input).is_err());
    }

    struct RecordingWriter(Arc<Mutex<Option<NewMemoryEntry>>>);

    #[async_trait::async_trait]
    impl MemoryWriter for RecordingWriter {
        async fn write(&self, record: NewMemoryEntry) -> Result<String, MemoryError> {
            *self.0.lock().unwrap() = Some(record);
            Ok("serialized-write".into())
        }
    }

    #[tokio::test]
    async fn memory_write_uses_the_attached_writer_boundary() {
        let captured = Arc::new(Mutex::new(None));
        let tool = MemoryWriteTool::new();
        tool.attach_writer(Arc::new(RecordingWriter(Arc::clone(&captured))));
        let mut metadata = ToolMetadata::default();
        let mut context = ToolContext {
            cwd: Path::new("/workspace"),
            metadata: &mut metadata,
        };

        let result = tool
            .execute(
                serde_json::json!({"content": "remember this", "scope": "private"}),
                &mut context,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.output, "saved memory serialized-write");
        let record = captured.lock().unwrap().clone().unwrap();
        assert_eq!(record.body, "remember this");
        assert_eq!(record.scope, crate::memory::MemoryScope::Private);
    }
}
