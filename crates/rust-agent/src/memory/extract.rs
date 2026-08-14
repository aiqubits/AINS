//! 持久记忆抽取 + 会话检查点（AINS_PLAN Phase 2.9，对齐 Harness
//! `services/memory_extract.py` + `services/session_memory.py`）。
//!
//! - 抽取：将近期对话与既有记忆清单交给模型，返回 ≤3 条 JSON 记录，
//!   宽松解析后写入 memdir（去重合并由 `MemdirStore::add_entry` 保证）；
//! - 会话检查点：结构化 `# Session Memory` 文档（当前状态 / 下一步 /
//!   已验证工作 / 活跃产物 / 近期对话），预算内截断。

use std::sync::Arc;

use futures::StreamExt;

use crate::error::AgentError;
use crate::kernel::messages::{ContentBlock, ConversationMessage, Role};
use crate::memory::kv::{KvStore, now_ms};
use crate::memory::memdir::{MemdirStore, MemoryScope, MemoryType, NewMemoryEntry, parse_iso_utc};
use crate::model_client::{ModelClient, ModelRequest, ModelStreamEvent};

/// 抽取 system prompt（逐字对齐基线 `EXTRACTION_SYSTEM_PROMPT`，Harness 名称保留）。
pub const EXTRACTION_SYSTEM_PROMPT: &str = "You maintain Harness durable memory.\nSave only stable, future-useful facts that are not derivable from current files,\ngit history, or documentation. Prefer updating existing memories conceptually\nover duplicating them. Do not save secrets. If nothing is worth saving, return\n{\"memories\": []}.\n";

/// 单次抽取最多保存的记录数（基线 `max_records=3`）。
pub const MAX_EXTRACT_RECORDS: usize = 3;
/// 抽取请求输出 token 上限（基线 2048）。
pub const EXTRACT_MAX_OUTPUT_TOKENS: u32 = 2048;
/// 抽取时纳入的最近消息数（基线 12）。
pub const TRANSCRIPT_MAX_MESSAGES: usize = 12;
/// 单条消息文本预算（基线 1200 字符）。
pub const TRANSCRIPT_MESSAGE_CHAR_CAP: usize = 1200;
/// 记忆清单最大文件数（基线 80）。
pub const MANIFEST_MAX_FILES: usize = 80;

/// 会话检查点字符预算（基线 `MAX_SESSION_MEMORY_CHARS`）。
pub const MAX_SESSION_MEMORY_CHARS: usize = 12_000;
/// 近期对话最大行数（基线 `MAX_RECENT_LINES`）。
pub const MAX_RECENT_LINES: usize = 80;
/// 会话检查点在 kv 表中的键。
pub const SESSION_MEMORY_KEY: &str = "memdir/session_memory.md";
/// 检查点截断标记（逐字对齐基线）。
pub const SESSION_TRUNCATION_MARKER: &str =
    "\n\n> Session memory was truncated to stay within budget.\n";

/// 抽取结果。
#[derive(Debug, Clone, Default)]
pub struct ExtractionOutcome {
    /// 本次写入/刷新的条目文件名。
    pub saved: Vec<String>,
    /// 跳过原因（None 表示执行了抽取）。
    pub skipped: Option<String>,
}

/// 记忆抽取器：memdir 存储 + 模型客户端。
pub struct MemoryExtractor {
    store: MemdirStore,
    model: Arc<dyn ModelClient>,
}

impl MemoryExtractor {
    pub fn new(store: MemdirStore, model: Arc<dyn ModelClient>) -> Self {
        Self { store, model }
    }

    /// 条件抽取：消息不足或本会话已写过记忆则跳过（基线 gating 语义）。
    pub async fn maybe_extract(
        &self,
        messages: &[ConversationMessage],
        memory_writes_since_last: bool,
    ) -> Result<ExtractionOutcome, AgentError> {
        if messages.len() < 2 {
            return Ok(ExtractionOutcome {
                saved: Vec::new(),
                skipped: Some("not enough messages".to_string()),
            });
        }
        if memory_writes_since_last {
            return Ok(ExtractionOutcome {
                saved: Vec::new(),
                skipped: Some("memory already updated since last extraction".to_string()),
            });
        }
        self.extract(messages).await
    }

    /// 执行抽取：清单 + 转写 → 模型 → 宽松 JSON 解析 → memdir 写入。
    pub async fn extract(
        &self,
        messages: &[ConversationMessage],
    ) -> Result<ExtractionOutcome, AgentError> {
        let manifest = self.build_manifest().await?;
        let transcript = format_transcript(messages);
        let prompt = format!(
            "Existing memory files:\n{}\n\nRecent conversation:\n{}\n\nReturn JSON only: {{\"memories\": [{{\"title\": str, \"content\": str, \"description\": str, \"type\": \"user|feedback|project|reference\", \"scope\": \"private|project|team\"}}]}} with at most {MAX_EXTRACT_RECORDS} records.",
            if manifest.is_empty() {
                "(none)".to_string()
            } else {
                manifest.join("\n")
            },
            transcript
        );

        let request = ModelRequest {
            model: None,
            messages: vec![ConversationMessage::from_user_text(prompt)],
            system_prompt: Some(EXTRACTION_SYSTEM_PROMPT.to_string()),
            max_output_tokens: EXTRACT_MAX_OUTPUT_TOKENS,
            tools: Vec::new(),
        };
        let mut stream = self.model.stream_response(request).await?;
        let mut response_text: Option<String> = None;
        while let Some(event) = stream.next().await {
            if let ModelStreamEvent::Complete { message, .. } = event {
                response_text = Some(message.text());
            }
        }
        // 流在 Complete 前终止（重试耗尽等）：与“模型判定无可保存”
        // 区分开，保持 gating 遥测可信（skipped=None 仅表示抽取完整执行）。
        let Some(response_text) = response_text else {
            return Ok(ExtractionOutcome {
                saved: Vec::new(),
                skipped: Some("model stream ended without completion".to_string()),
            });
        };

        let records = parse_memory_records(&response_text);
        let mut saved = Vec::new();
        for record in records.into_iter().take(MAX_EXTRACT_RECORDS) {
            let filename = self.store.add_entry(record).await?;
            saved.push(filename);
        }
        Ok(ExtractionOutcome {
            saved,
            skipped: None,
        })
    }

    /// 既有记忆清单行：`[{type}] {path} ({age}) - {desc}`（基线 ≤80 文件）。
    async fn build_manifest(&self) -> Result<Vec<String>, AgentError> {
        let now = now_ms();
        let entries = self.store.scan(MANIFEST_MAX_FILES).await?;
        Ok(entries
            .iter()
            .map(|e| {
                format!(
                    "[{}] {} ({}) - {}",
                    e.memory_type.as_str(),
                    e.filename,
                    format_age(parse_iso_utc(&e.updated_at), now),
                    e.description
                )
            })
            .collect())
    }
}

fn format_age(updated_ms: Option<i64>, now_ms: i64) -> String {
    let Some(updated) = updated_ms else {
        return "unknown age".to_string();
    };
    let days = ((now_ms - updated).max(0)) / (24 * 3600 * 1000);
    if days == 0 {
        "today".to_string()
    } else {
        format!("{days}d")
    }
}

/// 消息转写（基线格式）：最近 12 条，每条 1200 字符预算；
/// 纯文本 → `{role}: {text}`；工具调用 → `{role}: tool calls -> {names}`；
/// 其他 → `{role}: [non-text content]`。
pub fn format_transcript(messages: &[ConversationMessage]) -> String {
    let start = messages.len().saturating_sub(TRANSCRIPT_MAX_MESSAGES);
    let mut lines = Vec::new();
    for message in &messages[start..] {
        let role = role_name(message.role);
        let text = message.text();
        let line = if !text.trim().is_empty() {
            let capped: String = text.chars().take(TRANSCRIPT_MESSAGE_CHAR_CAP).collect();
            format!("{role}: {}", capped.trim())
        } else {
            let tool_names: Vec<String> = message.tool_uses().into_iter().map(|u| u.name).collect();
            if !tool_names.is_empty() {
                format!("{role}: tool calls -> {}", tool_names.join(", "))
            } else {
                format!("{role}: [non-text content]")
            }
        };
        lines.push(line);
    }
    lines.join("\n")
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// 宽松 JSON 解析：截取首个 `{` 到末个 `}`，逐记录宽松映射字段；
/// 解析失败或结构不符返回空列表（不视为错误）。
pub fn parse_memory_records(response: &str) -> Vec<NewMemoryEntry> {
    let Some(start) = response.find('{') else {
        return Vec::new();
    };
    let Some(end) = response.rfind('}') else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&response[start..=end]) else {
        return Vec::new();
    };
    let Some(records) = value.get("memories").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    records
        .iter()
        .filter_map(|record| {
            let get = |keys: &[&str]| -> String {
                keys.iter()
                    .find_map(|k| record.get(*k).and_then(|v| v.as_str()))
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let body = get(&["content", "body", "text"]);
            if body.is_empty() {
                return None;
            }
            let title = {
                let t = get(&["title", "name"]);
                if t.is_empty() {
                    body.chars().take(60).collect()
                } else {
                    t
                }
            };
            Some(NewMemoryEntry {
                title,
                description: get(&["description"]),
                memory_type: MemoryType::parse_lenient(&get(&["type"])),
                scope: MemoryScope::parse_lenient(&get(&["scope"])),
                importance: record
                    .get("importance")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0),
                source: "memory_extract".to_string(),
                ttl_days: record.get("ttl_days").and_then(|v| v.as_i64()).unwrap_or(0),
                tags: record
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                body,
            })
        })
        .collect()
}

// ── 会话检查点（对齐基线 services/session_memory）──

/// 检查点输入。
#[derive(Debug, Clone, Default)]
pub struct SessionCheckpoint {
    pub current_state: String,
    pub next_step: Option<String>,
    pub verified_work: Vec<String>,
    /// 活跃产物路径，只保留最后 10 个。
    pub active_artifacts: Vec<String>,
}

/// 构建 `# Session Memory` 文档（预算内截断，超限追加截断标记）。
pub fn build_session_memory(
    checkpoint: &SessionCheckpoint,
    messages: &[ConversationMessage],
) -> String {
    let mut doc = String::from("# Session Memory\n\n## Current State\n");
    doc.push_str(checkpoint.current_state.trim());
    doc.push('\n');

    if let Some(next_step) = &checkpoint.next_step
        && !next_step.trim().is_empty()
    {
        doc.push_str("\n## Next Step\n");
        doc.push_str(next_step.trim());
        doc.push('\n');
    }

    if !checkpoint.verified_work.is_empty() {
        doc.push_str("\n## Verified Work\n");
        for item in &checkpoint.verified_work {
            doc.push_str(&format!("- {}\n", item.trim()));
        }
    }

    if !checkpoint.active_artifacts.is_empty() {
        doc.push_str("\n## Active Artifacts\n");
        let start = checkpoint.active_artifacts.len().saturating_sub(10);
        for artifact in &checkpoint.active_artifacts[start..] {
            doc.push_str(&format!("- {}\n", artifact.trim()));
        }
    }

    doc.push_str("\n## Recent Conversation\n");
    let mut lines = Vec::new();
    for message in messages {
        let role = role_name(message.role);
        let text = message.text();
        if !text.trim().is_empty() {
            let capped: String = text.chars().take(220).collect();
            lines.push(format!("- {role}: {}", capped.trim()));
        } else {
            let names: Vec<String> = message
                .tool_uses()
                .into_iter()
                .map(|u| u.name)
                .take(6)
                .collect();
            if !names.is_empty() {
                lines.push(format!("- {role}: tool calls -> {}", names.join(", ")));
            } else if message
                .content
                .iter()
                .any(|b| !matches!(b, ContentBlock::Text { .. }))
            {
                lines.push(format!("- {role}: [non-text content]"));
            }
        }
    }
    let start = lines.len().saturating_sub(MAX_RECENT_LINES);
    for line in &lines[start..] {
        doc.push_str(line);
        doc.push('\n');
    }

    if doc.chars().count() > MAX_SESSION_MEMORY_CHARS {
        let budget = MAX_SESSION_MEMORY_CHARS.saturating_sub(SESSION_TRUNCATION_MARKER.len());
        let truncated: String = doc.chars().take(budget).collect();
        doc = format!("{}{SESSION_TRUNCATION_MARKER}", truncated.trim_end());
    }
    doc
}

/// 持久化会话检查点到 kv 表。
pub async fn save_session_checkpoint(
    kv: &Arc<dyn KvStore>,
    document: &str,
) -> Result<(), AgentError> {
    kv.set(
        SESSION_MEMORY_KEY,
        &serde_json::Value::String(document.to_string()),
        None,
    )
    .await?;
    Ok(())
}

/// 读取会话检查点（不存在返回 None）。
pub async fn load_session_checkpoint(kv: &Arc<dyn KvStore>) -> Result<Option<String>, AgentError> {
    Ok(kv
        .get(SESSION_MEMORY_KEY)
        .await?
        .and_then(|v| v.as_str().map(|s| s.to_string())))
}
