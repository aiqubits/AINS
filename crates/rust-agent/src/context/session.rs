//! 会话持久化（Phase 5.4，对齐 OpenHarness `services/session_storage.py`）。
//!
//! 快照存储走 `KvStore`（Native = redb / Web = IndexedDB，双端一致），而非
//! 基线的文件系统。对齐要点：
//! - 落盘前 `sanitize_conversation_messages`（丢空 assistant、修剪悬空 tool_use）；
//! - `tool_metadata` 白名单：结构化状态字段全量保留，自由 `extra` 仅保留白名单键；
//! - 按项目隔离：key 前缀 = `session/{basename}-{sha256(cwd)[:12]}/`；
//! - latest + 按 id 双写：先写按 id 条目（完整快照），再更新 latest 指针，
//!   KvStore 单键 set 各自原子（redb 事务 / IndexedDB 事务）。

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::AgentError;
use crate::kernel::messages::{ConversationMessage, sanitize_conversation_messages};
use crate::memory::{KvStore, now_ms};
use crate::model_client::UsageSnapshot;
use crate::tools::ToolMetadata;

/// 快照 summary 的最大字符数（对齐基线首条 user 消息前 80 字符）。
pub const SUMMARY_MAX_CHARS: usize = 80;
/// `list` 默认返回的最大条目数（对齐基线 `limit = 20`）。
pub const DEFAULT_LIST_LIMIT: usize = 20;
/// session_id 的十六进制长度（对齐基线 uuid4 hex 前 12 位）。
pub const SESSION_ID_HEX_LEN: usize = 12;

/// `ToolMetadata.extra` 中允许持久化的键白名单（对齐基线
/// `_PERSISTED_TOOL_METADATA_KEYS` 的非结构化子集；结构化字段
/// read_files/invoked_skills/work_log 等已是受治理状态，全量保留）。
pub const PERSISTED_EXTRA_KEYS: [&str; 6] = [
    "permission_mode",
    "task_focus_state",
    "async_agent_state",
    "async_agent_tasks",
    "compact_checkpoints",
    "compact_last",
];

/// 会话快照（对齐基线 `save_session_snapshot` payload 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    pub messages: Vec<ConversationMessage>,
    #[serde(default)]
    pub usage: UsageSnapshot,
    #[serde(default)]
    pub tool_metadata: ToolMetadata,
    pub created_at_ms: i64,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub message_count: usize,
}

/// 会话列表摘要（`list` 返回；仅携带摘要字段，不向调用方暴露完整
/// 消息；每条目仍需完整反序列化后丢弃消息，条数受 limit 约束）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub summary: String,
    pub created_at_ms: i64,
    pub message_count: usize,
}

/// 会话快照的构造输入（`session_id` 为空时自动生成）。
#[derive(Debug, Clone, Default)]
pub struct SessionSaveInput {
    pub session_id: Option<String>,
    pub cwd: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub usage: UsageSnapshot,
    pub tool_metadata: ToolMetadata,
}

/// 会话持久化存储（KvStore 后端）。
pub struct SessionStore {
    kv: Arc<dyn KvStore>,
    /// 可选的持久化 owner 分区。Web 的 IndexedDB 在同一浏览器 profile 中
    /// 被所有账户共享，不能只用 cwd 作为隔离键；Native 保持空以兼容既有
    /// 单用户 session key。
    owner_scope: Option<String>,
}

impl SessionStore {
    pub fn new(kv: Arc<dyn KvStore>) -> Self {
        Self {
            kv,
            owner_scope: None,
        }
    }

    /// 构造按不可逆 owner key 分区的 session store。空 scope 退化为
    /// [`Self::new`]，防止调用方意外写入 `owner//` 前缀。
    pub fn new_scoped(kv: Arc<dyn KvStore>, owner_scope: impl Into<String>) -> Self {
        let owner_scope = owner_scope.into();
        Self {
            kv,
            owner_scope: (!owner_scope.trim().is_empty()).then_some(owner_scope),
        }
    }

    fn scoped_slug(&self, cwd: &str) -> String {
        let project = project_slug(cwd);
        match &self.owner_scope {
            Some(owner) => format!("owner/{owner}/{project}"),
            None => project,
        }
    }

    /// 持久化快照：sanitize → 白名单过滤 → 双写（按 id + latest）。返回 session_id。
    pub async fn save(&self, input: SessionSaveInput) -> Result<String, AgentError> {
        let now = now_ms();
        let session_id = input
            .session_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| generate_session_id(now, &input.cwd));

        let messages = sanitize_conversation_messages(input.messages);
        let summary = extract_summary(&messages);
        let snapshot = SessionSnapshot {
            session_id: session_id.clone(),
            cwd: input.cwd.clone(),
            model: input.model,
            system_prompt: input.system_prompt,
            message_count: messages.len(),
            summary,
            messages,
            usage: input.usage,
            tool_metadata: persistable_tool_metadata(&input.tool_metadata),
            created_at_ms: now,
        };
        let value = serde_json::to_value(&snapshot)
            .map_err(|e| AgentError::Model(format!("session snapshot encode failed: {e}")))?;

        let slug = self.scoped_slug(&input.cwd);
        // 先写按 id 的完整条目，再更新 latest 指针（崩溃时按 id 条目仍完整）。
        self.kv
            .set(&entry_key(&slug, &session_id), &value, None)
            .await?;
        self.kv.set(&latest_key(&slug), &value, None).await?;
        Ok(session_id)
    }

    /// 读取项目最近快照（latest 指针）；回载时再 sanitize 一次。
    pub async fn load_latest(&self, cwd: &str) -> Result<Option<SessionSnapshot>, AgentError> {
        let slug = self.scoped_slug(cwd);
        self.load_key(&latest_key(&slug)).await
    }

    /// 按 id 读取快照：先试命名条目，再回退 latest（id 匹配或字面 "latest"）。
    pub async fn load_by_id(
        &self,
        cwd: &str,
        session_id: &str,
    ) -> Result<Option<SessionSnapshot>, AgentError> {
        let slug = self.scoped_slug(cwd);
        if let Some(snapshot) = self.load_key(&entry_key(&slug, session_id)).await? {
            return Ok(Some(snapshot));
        }
        if let Some(latest) = self.load_key(&latest_key(&slug)).await?
            && (latest.session_id == session_id || session_id == "latest")
        {
            return Ok(Some(latest));
        }
        Ok(None)
    }

    /// 列出项目会话摘要（按 created_at 降序，截断 limit；latest 指针不重复计入）。
    ///
    /// 无法反序列化的损坏条目跳过而非整表失败（对齐基线 `list_sessions`
    /// 对 JSONDecodeError/OSError 逐条 continue）。
    pub async fn list(&self, cwd: &str, limit: usize) -> Result<Vec<SessionSummary>, AgentError> {
        let slug = self.scoped_slug(cwd);
        let prefix = entry_prefix(&slug);
        let keys = self.kv.list_prefix(&prefix).await?;
        let mut summaries = Vec::new();
        for key in keys {
            if let Ok(Some(snapshot)) = self.load_key(&key).await {
                summaries.push(SessionSummary {
                    session_id: snapshot.session_id,
                    summary: snapshot.summary,
                    created_at_ms: snapshot.created_at_ms,
                    message_count: snapshot.message_count,
                });
            }
        }
        summaries.sort_by_key(|s| std::cmp::Reverse(s.created_at_ms));
        summaries.truncate(limit);
        Ok(summaries)
    }

    /// 读取并反序列化单个快照 key（回载时对消息再 sanitize，对齐基线
    /// `_sanitize_snapshot_payload`：修剪快照后仍可能残留的悬空 tool 结构）。
    async fn load_key(&self, key: &str) -> Result<Option<SessionSnapshot>, AgentError> {
        let Some(value) = self.kv.get(key).await? else {
            return Ok(None);
        };
        let mut snapshot: SessionSnapshot = serde_json::from_value(value)
            .map_err(|e| AgentError::Model(format!("session snapshot decode failed: {e}")))?;
        snapshot.messages = sanitize_conversation_messages(snapshot.messages);
        snapshot.message_count = snapshot.messages.len();
        Ok(Some(snapshot))
    }
}

/// 项目隔离 slug：`{basename}-{sha256(cwd)[:12]}`（对齐基线目录命名，
/// 摘要算法用 crate 内已有的 sha256，不依赖文件系统 canonicalize，双端一致）。
pub fn project_slug(cwd: &str) -> String {
    let basename = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("root");
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(SESSION_ID_HEX_LEN / 2)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{basename}-{hex}")
}

fn entry_prefix(slug: &str) -> String {
    format!("session/{slug}/entry/")
}

fn entry_key(slug: &str, session_id: &str) -> String {
    format!("{}{session_id}", entry_prefix(slug))
}

fn latest_key(slug: &str) -> String {
    format!("session/{slug}/latest")
}

/// 生成 12 位十六进制 session_id（now_ms + 进程内单调计数 + cwd +
/// CSPRNG 熵的 sha256 摘要）。计数器消除单进程同毫秒同 cwd 两次 save
/// 的 id 碰撞；随机熵再隔离不同 Web 标签页/进程中各自从零开始的计数器，
/// 否则它们在同一毫秒初始化会静默覆盖同一按 id 快照。
///
/// 公开供 app 装配层在新会话预生成 session_id：与 `SessionStore::save`
/// 的自动生成路径同源，保证 MemoryService 的 checkpoint / digest / status
/// key 与后续快照落盘使用同一 id（避免字面量占位跨会话污染）。
pub fn generate_session_id(now_ms: i64, cwd: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut entropy = [0u8; 12];
    if let Err(error) = getrandom::getrandom(&mut entropy) {
        // Session IDs are not authentication secrets. Preserve the existing
        // availability behavior if a platform RNG is temporarily unavailable;
        // the counter still protects same-process writes, while this warning
        // makes the weaker cross-process collision resistance observable.
        tracing::warn!(error = %error, "session id RNG unavailable; using deterministic fallback");
    }
    session_id_from_components(now_ms, counter, cwd, entropy)
}

fn session_id_from_components(now_ms: i64, counter: u64, cwd: &str, entropy: [u8; 12]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(now_ms.to_le_bytes());
    hasher.update(counter.to_le_bytes());
    hasher.update(cwd.as_bytes());
    hasher.update(entropy);
    let digest = hasher.finalize();
    digest
        .iter()
        .take(SESSION_ID_HEX_LEN / 2)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 首条含非空文本的 user 消息，text 前 [`SUMMARY_MAX_CHARS`] 字符（对齐基线）。
fn extract_summary(messages: &[ConversationMessage]) -> String {
    use crate::kernel::messages::Role;
    for message in messages {
        if message.role == Role::User {
            let text = message.text();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(SUMMARY_MAX_CHARS).collect();
            }
        }
    }
    String::new()
}

/// 过滤 `tool_metadata`：结构化状态字段全量保留，`extra` 仅保留白名单键
/// （对齐基线只持久化白名单键，避免临时/敏感键写盘）。
fn persistable_tool_metadata(metadata: &ToolMetadata) -> ToolMetadata {
    let mut filtered = metadata.clone();
    filtered
        .extra
        .retain(|key, _| PERSISTED_EXTRA_KEYS.contains(&key.as_str()));
    filtered
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::kernel::messages::{ContentBlock, ConversationMessage, Role};
    use crate::memory::RedbKvStore;

    fn kv() -> Arc<dyn KvStore> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.redb");
        // tempdir 生命周期：泄漏到测试进程结束（redb 打开文件句柄即可）
        std::mem::forget(dir);
        Arc::new(RedbKvStore::open(&path).unwrap())
    }

    fn user_msg(text: &str) -> ConversationMessage {
        ConversationMessage::from_user_text(text)
    }

    fn assistant_msg(text: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[tokio::test]
    async fn save_and_load_latest_roundtrip() {
        let store = SessionStore::new(kv());
        let input = SessionSaveInput {
            cwd: "/proj/alpha".into(),
            model: Some("gpt-test".into()),
            messages: vec![user_msg("first goal"), assistant_msg("done")],
            ..Default::default()
        };
        let id = store.save(input).await.unwrap();
        assert_eq!(id.len(), SESSION_ID_HEX_LEN);

        let loaded = store.load_latest("/proj/alpha").await.unwrap().unwrap();
        assert_eq!(loaded.session_id, id);
        assert_eq!(loaded.model.as_deref(), Some("gpt-test"));
        assert_eq!(loaded.summary, "first goal");
        assert_eq!(loaded.message_count, 2);
    }

    #[tokio::test]
    async fn scoped_stores_isolate_same_workspace_between_owners() {
        let shared_kv = kv();
        let owner_a = SessionStore::new_scoped(Arc::clone(&shared_kv), "owner-a-hash");
        let owner_b = SessionStore::new_scoped(shared_kv, "owner-b-hash");
        owner_a
            .save(SessionSaveInput {
                cwd: "/ains-web".into(),
                messages: vec![user_msg("account A private chat")],
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(
            owner_b.load_latest("/ains-web").await.unwrap().is_none(),
            "a shared Web backend must not restore another owner's latest session"
        );
        assert!(
            owner_b
                .list("/ains-web", DEFAULT_LIST_LIMIT)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn save_sanitizes_dangling_tool_use_before_persist() {
        let store = SessionStore::new(kv());
        // 末尾悬空 tool_use（无配对 tool_result）应被 sanitize 修剪
        let dangling = ConversationMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "calc".into(),
                input: serde_json::json!({}),
            }],
        };
        let input = SessionSaveInput {
            cwd: "/proj/beta".into(),
            messages: vec![user_msg("hi"), dangling],
            ..Default::default()
        };
        store.save(input).await.unwrap();
        let loaded = store.load_latest("/proj/beta").await.unwrap().unwrap();
        // 悬空 assistant 被剔除，仅剩 user 消息
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].role, Role::User);
    }

    #[tokio::test]
    async fn tool_metadata_extra_whitelist_filtering() {
        let store = SessionStore::new(kv());
        let mut metadata = ToolMetadata::new();
        metadata.record_read_file("/a.rs");
        metadata
            .extra
            .insert("permission_mode".into(), serde_json::json!("plan"));
        metadata
            .extra
            .insert("secret_scratch".into(), serde_json::json!("should-drop"));
        let input = SessionSaveInput {
            cwd: "/proj/gamma".into(),
            messages: vec![user_msg("hi")],
            tool_metadata: metadata,
            ..Default::default()
        };
        store.save(input).await.unwrap();
        let loaded = store.load_latest("/proj/gamma").await.unwrap().unwrap();
        // 结构化字段保留
        assert_eq!(loaded.tool_metadata.read_files, vec!["/a.rs".to_string()]);
        // 白名单键保留，非白名单键丢弃
        assert!(loaded.tool_metadata.extra.contains_key("permission_mode"));
        assert!(!loaded.tool_metadata.extra.contains_key("secret_scratch"));
    }

    #[tokio::test]
    async fn load_by_id_falls_back_to_latest() {
        let store = SessionStore::new(kv());
        let id = store
            .save(SessionSaveInput {
                cwd: "/proj/delta".into(),
                messages: vec![user_msg("goal")],
                ..Default::default()
            })
            .await
            .unwrap();

        // 命名条目命中
        assert!(
            store
                .load_by_id("/proj/delta", &id)
                .await
                .unwrap()
                .is_some()
        );
        // 字面 "latest" 回退命中
        assert!(
            store
                .load_by_id("/proj/delta", "latest")
                .await
                .unwrap()
                .is_some()
        );
        // 未知 id 且与 latest 不匹配 → None
        assert!(
            store
                .load_by_id("/proj/delta", "deadbeef0000")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_orders_by_created_at_desc_and_isolates_projects() {
        let store = SessionStore::new(kv());
        store
            .save(SessionSaveInput {
                session_id: Some("aaaaaaaaaaaa".into()),
                cwd: "/proj/one".into(),
                messages: vec![user_msg("older")],
                ..Default::default()
            })
            .await
            .unwrap();
        // 确保 created_at 递增
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store
            .save(SessionSaveInput {
                session_id: Some("bbbbbbbbbbbb".into()),
                cwd: "/proj/one".into(),
                messages: vec![user_msg("newer")],
                ..Default::default()
            })
            .await
            .unwrap();
        // 另一项目的会话不应出现在本项目列表
        store
            .save(SessionSaveInput {
                cwd: "/proj/two".into(),
                messages: vec![user_msg("other project")],
                ..Default::default()
            })
            .await
            .unwrap();

        let list = store.list("/proj/one", DEFAULT_LIST_LIMIT).await.unwrap();
        assert_eq!(list.len(), 2);
        // 降序：newer 在前
        assert_eq!(list[0].session_id, "bbbbbbbbbbbb");
        assert_eq!(list[1].session_id, "aaaaaaaaaaaa");
    }

    #[tokio::test]
    async fn list_skips_corrupted_entries_instead_of_failing() {
        // 回归：损坏条目曾经 `?` 传播导致 list 整表失败；应逐条跳过
        // （对齐基线 list_sessions 对 JSONDecodeError 的 continue）。
        let kv = kv();
        let store = SessionStore::new(Arc::clone(&kv));
        store
            .save(SessionSaveInput {
                cwd: "/proj/epsilon".into(),
                messages: vec![user_msg("valid session")],
                ..Default::default()
            })
            .await
            .unwrap();
        // 直接向同前缀写入无法反序列化为快照的值
        let slug = project_slug("/proj/epsilon");
        kv.set(
            &format!("session/{slug}/entry/corrupted0000"),
            &serde_json::json!("not a snapshot"),
            None,
        )
        .await
        .unwrap();

        let list = store
            .list("/proj/epsilon", DEFAULT_LIST_LIMIT)
            .await
            .unwrap();
        assert_eq!(list.len(), 1, "corrupted entry must be skipped, not fatal");
        assert_eq!(list[0].summary, "valid session");
    }

    #[test]
    fn project_slug_is_deterministic_and_isolating() {
        let a = project_slug("/proj/alpha");
        let b = project_slug("/proj/beta");
        assert!(a.starts_with("alpha-"));
        assert!(b.starts_with("beta-"));
        assert_ne!(a, b);
        // 确定性
        assert_eq!(a, project_slug("/proj/alpha"));
    }

    #[test]
    fn generate_session_id_unique_within_same_millisecond() {
        // 回归：同毫秒 + 同 cwd 曾产生相同 id，导致按 id 快照静默互相覆盖
        let a = generate_session_id(42, "/proj/same");
        let b = generate_session_id(42, "/proj/same");
        assert_eq!(a.len(), SESSION_ID_HEX_LEN);
        assert_eq!(b.len(), SESSION_ID_HEX_LEN);
        assert_ne!(
            a, b,
            "same-millisecond saves must yield distinct session ids"
        );
    }

    #[test]
    fn session_id_entropy_separates_independent_tab_initializations() {
        // Independent browser tabs have independent statics, so both can
        // begin with the same timestamp, cwd, and local counter. Entropy must
        // make those otherwise identical inputs produce different session IDs.
        let a = session_id_from_components(42, 0, "/proj/same", [1; 12]);
        let b = session_id_from_components(42, 0, "/proj/same", [2; 12]);
        assert_ne!(a, b);
    }

    #[test]
    fn extract_summary_takes_first_user_text_capped() {
        let long = "x".repeat(200);
        let messages = vec![assistant_msg("ignored"), user_msg(&long)];
        let summary = extract_summary(&messages);
        assert_eq!(summary.chars().count(), SUMMARY_MAX_CHARS);
    }
}
