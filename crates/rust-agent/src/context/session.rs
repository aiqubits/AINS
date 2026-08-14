//! 会话持久化（Phase 5.4，对齐 Harness `services/session_storage.py`）。
//!
//! 快照存储走 `KvStore`（Native = redb / Web = IndexedDB，双端一致），而非
//! 基线的文件系统。对齐要点：
//! - 落盘前 `sanitize_conversation_messages`（丢空 assistant、修剪悬空 tool_use）；
//! - `tool_metadata` 白名单：结构化状态字段全量保留，自由 `extra` 仅保留白名单键；
//! - 按项目隔离：key 前缀 = `session/{basename}-{sha256(cwd)[:12]}/`；
//! - 每个项目只保留一个 canonical current snapshot；session_id 仅用于
//!   清空屏障、checkpoint 与长期记忆的来源追踪，不构成可恢复的历史列表。

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AgentError, MemoryError};
use crate::kernel::messages::{ConversationMessage, sanitize_conversation_messages};
use crate::memory::{KvStore, now_ms};
use crate::model_client::UsageSnapshot;
use crate::tools::ToolMetadata;

/// session_id 的十六进制长度（对齐基线 uuid4 hex 前 12 位）。
pub const SESSION_ID_HEX_LEN: usize = 12;

/// 已清空会话的持久化屏障。所有旧 session 的 snapshot/checkpoint/memory 写入
/// 都必须检查它，避免另一个标签页或恢复实例在清空后重新落盘。
pub(crate) fn cleared_session_key(owner_key: &str, project_key: &str, session_id: &str) -> String {
    format!("memory/cleared_sessions/{owner_key}/{project_key}/{session_id}")
}

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
}

/// 清空会话的可观测结果。
///
/// `clear_current` 返回 `Ok` 即表示 tombstone 已持久化，会话已在读取侧
/// 不可见且调用方可安全切换到新 session。`cleanup_failures` 仅表示对旧
/// 物理记录的尽力回收尚有失败，不能据此继续使用已清空的 session。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionClearOutcome {
    pub cleanup_failures: Vec<String>,
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

    /// MemoryService 对 owner 一律使用不可逆摘要。Native 的 session snapshot
    /// 为兼容旧数据仍保留无 owner 前缀的键，但其 checkpoint/status/tombstone
    /// 必须使用相同的 memory owner key，否则清空会漏删这些会话工件。
    fn memory_owner_key(&self) -> String {
        self.owner_scope
            .clone()
            .unwrap_or_else(|| crate::memory::owner_key_for_id("local"))
    }

    /// 跨标签页的项目级会话操作锁名。产品定义为每个终端/项目只有一个
    /// 当前历史，保存和清空必须争用同一把锁，不能再按 session_id 分锁。
    pub fn operation_lock_name(&self, cwd: &str) -> String {
        format!(
            "ains-current-session-v1/{}/{}",
            self.memory_owner_key(),
            project_slug(cwd)
        )
    }

    async fn session_was_cleared(&self, cwd: &str, session_id: &str) -> Result<bool, AgentError> {
        let owner_key = self.memory_owner_key();
        Ok(self
            .kv
            .get(&cleared_session_key(
                &owner_key,
                &project_slug(cwd),
                session_id,
            ))
            .await?
            .is_some())
    }

    async fn reject_cleared_session(&self, cwd: &str, session_id: &str) -> Result<(), AgentError> {
        if self.session_was_cleared(cwd, session_id).await? {
            return Err(AgentError::Memory(MemoryError::Storage(
                "refusing snapshot write for a cleared session".into(),
            )));
        }
        Ok(())
    }

    /// 清空屏障可能在 current snapshot 的写入期间落下。拒绝写入时仅当
    /// current 仍指向同一 session 时删除它，避免误删另一个终端的当前会话。
    async fn reject_and_cleanup_cleared_snapshot(
        &self,
        cwd: &str,
        slug: &str,
        session_id: &str,
    ) -> Result<(), AgentError> {
        if !self.session_was_cleared(cwd, session_id).await? {
            return Ok(());
        }
        let current = current_key(slug);
        if let Ok(Some(snapshot)) = self.load_key(&current).await
            && snapshot.session_id == session_id
        {
            let _ = self.kv.delete(&current).await;
        }
        Err(AgentError::Memory(MemoryError::Storage(
            "refusing snapshot write for a cleared session".into(),
        )))
    }

    /// 持久化唯一的当前快照：sanitize → 白名单过滤 → 单键覆盖。返回的
    /// session_id 只标识清空边界，不能用于恢复历史会话。
    pub async fn save(&self, input: SessionSaveInput) -> Result<String, AgentError> {
        let now = now_ms();
        let session_id = input
            .session_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| generate_session_id(now, &input.cwd));
        self.reject_cleared_session(&input.cwd, &session_id).await?;
        let slug = self.scoped_slug(&input.cwd);
        let current = current_key(&slug);

        let messages = sanitize_conversation_messages(input.messages);
        let snapshot = SessionSnapshot {
            session_id: session_id.clone(),
            cwd: input.cwd.clone(),
            model: input.model,
            system_prompt: input.system_prompt,
            messages,
            usage: input.usage,
            tool_metadata: persistable_tool_metadata(&input.tool_metadata),
        };
        let value = serde_json::to_value(&snapshot)
            .map_err(|e| AgentError::Model(format!("session snapshot encode failed: {e}")))?;

        // 新版本不再读取或写入历史 entry；保存时顺便清除旧版本残留。
        self.kv.delete_prefix(&legacy_entry_prefix(&slug)).await?;
        self.kv.set(&current, &value, None).await?;
        // 单键写之后仍须重查：另一个标签页可能恰好在 set 期间完成清空。
        self.reject_and_cleanup_cleared_snapshot(&input.cwd, &slug, &session_id)
            .await?;
        Ok(session_id)
    }

    /// 读取项目唯一的当前快照；回载时再 sanitize 一次。
    pub async fn load_current(&self, cwd: &str) -> Result<Option<SessionSnapshot>, AgentError> {
        let slug = self.scoped_slug(cwd);
        let snapshot = self.load_key(&current_key(&slug)).await?;
        match snapshot {
            Some(snapshot) if self.session_was_cleared(cwd, &snapshot.session_id).await? => {
                Ok(None)
            }
            snapshot => Ok(snapshot),
        }
    }

    /// 删除当前会话的持久化快照。
    ///
    /// `current` 只有仍指向指定会话时才删除，避免陈旧界面误删后来创建的
    /// 当前会话。旧版本遗留的历史 entry 会在这里一并清除。
    pub async fn clear_current(
        &self,
        cwd: &str,
        session_id: &str,
    ) -> Result<SessionClearOutcome, AgentError> {
        let slug = self.scoped_slug(cwd);
        // 会话快照使用 `scoped_slug`，但 MemoryService 的 checkpoint/status
        // 使用独立的 owner + project_key 两级键；Web 的 scoped slug 中已包含
        // owner，不能直接复用，否则会漏删这些会话工件。
        let project_key = project_slug(cwd);
        let current = current_key(&slug);

        // tombstone 是整个清空操作的提交点。它无法建立时不能删除任何历史，
        // 否则旧标签页仍可回写；已建立后即使物理回收部分失败，也必须让
        // 调用方切换到新会话，避免 UI 保留一个已经不可再持久化的旧 session。
        if !self.session_was_cleared(cwd, session_id).await? {
            self.kv
                .set(
                    &cleared_session_key(&self.memory_owner_key(), &project_key, session_id),
                    &serde_json::Value::Bool(true),
                    None,
                )
                .await?;
        }

        // 各逻辑键没有跨表事务；即使其中一个删除失败，也继续尝试剩余目标，
        // 尽可能回收物理记录，并把失败交给 UI 以警告用户。
        let mut failures = Vec::new();
        let owner_key = self.memory_owner_key();
        match self.load_key(&current).await {
            Ok(Some(snapshot)) if snapshot.session_id == session_id => {
                if let Err(error) = self.kv.delete(&current).await {
                    failures.push(format!("current: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(format!("read current: {error}")),
        }
        if let Err(error) = self.kv.delete_prefix(&legacy_entry_prefix(&slug)).await {
            failures.push(format!("legacy entries: {error}"));
        }
        // checkpoint/status 与会话快照同在 kv 表，但它们属于当前会话历史，
        // 即使 MemoryService 因配置或初始化失败不可用也必须一并清除。
        if let Err(error) = self
            .kv
            .delete(&format!(
                "memory/checkpoints/{owner_key}/{project_key}/{session_id}.md"
            ))
            .await
        {
            failures.push(format!("checkpoint: {error}"));
        }
        if let Err(error) = self
            .kv
            .delete_prefix(&format!(
                "memory/status/{owner_key}/{project_key}/{session_id}/"
            ))
            .await
        {
            failures.push(format!("status: {error}"));
        }
        Ok(SessionClearOutcome {
            cleanup_failures: failures,
        })
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

/// Pre-single-history key prefix. It is only used to erase data written by
/// previous releases; no production read/write path treats these as sessions.
fn legacy_entry_prefix(slug: &str) -> String {
    format!("session/{slug}/entry/")
}

fn current_key(slug: &str) -> String {
    // Retain the physical key name for a zero-copy migration from the prior
    // release; its semantics are now strictly one current snapshot.
    format!("session/{slug}/latest")
}

/// 生成 12 位十六进制 session_id（now_ms + 进程内单调计数 + cwd +
/// CSPRNG 熵的 sha256 摘要）。计数器消除单进程同毫秒同 cwd 两次初始化
/// 的 id 碰撞；随机熵再隔离不同 Web 标签页/进程中各自从零开始的计数器，
/// 使它们的清空屏障、checkpoint 与长期记忆来源不会相互混淆。
///
/// 公开供 app 装配层在清空后的新运行边界预生成 session_id：与 `SessionStore::save`
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::error::MemoryError;
    use crate::kernel::messages::{ContentBlock, ConversationMessage, Role};
    use crate::memory::RedbKvStore;

    /// Injects the clear tombstone immediately after the current key is written,
    /// reproducing the final single-key-write race in `SessionStore::save`.
    struct TombstoneAfterCurrentKv {
        inner: Box<dyn KvStore>,
        tombstone_key: String,
        armed: AtomicBool,
    }

    impl TombstoneAfterCurrentKv {
        fn new(inner: impl KvStore + 'static, tombstone_key: String) -> Self {
            Self {
                inner: Box::new(inner),
                tombstone_key,
                armed: AtomicBool::new(true),
            }
        }
    }

    #[async_trait::async_trait]
    impl KvStore for TombstoneAfterCurrentKv {
        async fn get(&self, key: &str) -> Result<Option<serde_json::Value>, MemoryError> {
            self.inner.get(key).await
        }

        async fn set(
            &self,
            key: &str,
            value: &serde_json::Value,
            ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            self.inner.set(key, value, ttl).await?;
            if key.ends_with("/latest") && self.armed.swap(false, Ordering::SeqCst) {
                self.inner
                    .set(&self.tombstone_key, &serde_json::Value::Bool(true), None)
                    .await?;
            }
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), MemoryError> {
            self.inner.delete(key).await
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            self.inner.list_prefix(prefix).await
        }
    }

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
    async fn save_and_load_current_roundtrip() {
        let store = SessionStore::new(kv());
        let input = SessionSaveInput {
            cwd: "/proj/alpha".into(),
            model: Some("gpt-test".into()),
            messages: vec![user_msg("first goal"), assistant_msg("done")],
            ..Default::default()
        };
        let id = store.save(input).await.unwrap();
        assert_eq!(id.len(), SESSION_ID_HEX_LEN);

        let loaded = store.load_current("/proj/alpha").await.unwrap().unwrap();
        assert_eq!(loaded.session_id, id);
        assert_eq!(loaded.model.as_deref(), Some("gpt-test"));
        assert_eq!(loaded.messages.len(), 2);
    }

    #[tokio::test]
    async fn save_cleans_current_when_clear_happens_after_the_intermediate_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-race.redb");
        let cwd = "/proj/final-clear-race";
        let session_id = "clear-race-id";
        let owner_key = crate::memory::owner_key_for_id("local");
        let tombstone = cleared_session_key(&owner_key, &project_slug(cwd), session_id);
        let kv: Arc<dyn KvStore> = Arc::new(TombstoneAfterCurrentKv::new(
            RedbKvStore::open(&path).unwrap(),
            tombstone,
        ));
        let store = SessionStore::new(Arc::clone(&kv));

        assert!(
            store
                .save(SessionSaveInput {
                    session_id: Some(session_id.into()),
                    cwd: cwd.into(),
                    messages: vec![user_msg("must not survive clear")],
                    ..Default::default()
                })
                .await
                .is_err()
        );

        let slug = project_slug(cwd);
        assert!(
            kv.get(&current_key(&slug)).await.unwrap().is_none(),
            "the post-clear current snapshot must be cleaned up"
        );
    }

    #[tokio::test]
    async fn clear_current_removes_the_snapshot_and_legacy_entries() {
        let kv = kv();
        let owner_key = "owner-a-hash";
        let store = SessionStore::new_scoped(Arc::clone(&kv), owner_key);
        let cwd = "/proj/clear";
        let current = store
            .save(SessionSaveInput {
                session_id: Some("bbbbbbbbbbbb".into()),
                cwd: cwd.into(),
                messages: vec![user_msg("current")],
                ..Default::default()
            })
            .await
            .unwrap();

        let slug = project_slug(cwd);
        kv.set(
            &format!(
                "{}old-session",
                legacy_entry_prefix(&format!("owner/{owner_key}/{slug}"))
            ),
            &serde_json::json!("obsolete history entry"),
            None,
        )
        .await
        .unwrap();
        kv.set(
            &format!("memory/checkpoints/{owner_key}/{slug}/{current}.md"),
            &serde_json::json!("recent conversation"),
            None,
        )
        .await
        .unwrap();
        kv.set(
            &format!("memory/status/{owner_key}/{slug}/{current}/last_success_digest"),
            &serde_json::json!("digest"),
            None,
        )
        .await
        .unwrap();

        store.clear_current(cwd, &current).await.unwrap();

        assert!(store.load_current(cwd).await.unwrap().is_none());
        assert!(
            kv.list_prefix(&legacy_entry_prefix(&format!("owner/{owner_key}/{slug}")))
                .await
                .unwrap()
                .is_empty(),
            "legacy history entries must be removed during clear"
        );
        assert!(
            kv.get(&format!(
                "memory/checkpoints/{owner_key}/{slug}/{current}.md"
            ))
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            kv.list_prefix(&format!("memory/status/{owner_key}/{slug}/{current}/"))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .save(SessionSaveInput {
                    session_id: Some(current.clone()),
                    cwd: cwd.into(),
                    messages: vec![user_msg("stale tab must not recreate")],
                    ..Default::default()
                })
                .await
                .is_err(),
            "tombstone 必须拒绝另一个标签页对已清空 session 的旧快照回写"
        );
    }

    #[tokio::test]
    async fn native_clear_uses_memory_services_hashed_local_owner_key() {
        let kv = kv();
        let store = SessionStore::new(Arc::clone(&kv));
        let cwd = "/proj/native-clear";
        let session_id = "native-current";
        let project_key = project_slug(cwd);
        let memory_owner_key = crate::memory::owner_key_for_id("local");
        store
            .save(SessionSaveInput {
                session_id: Some(session_id.into()),
                cwd: cwd.into(),
                messages: vec![user_msg("current")],
                ..Default::default()
            })
            .await
            .unwrap();
        kv.set(
            &format!("memory/checkpoints/{memory_owner_key}/{project_key}/{session_id}.md"),
            &serde_json::json!("checkpoint"),
            None,
        )
        .await
        .unwrap();
        kv.set(
            &format!("memory/status/{memory_owner_key}/{project_key}/{session_id}/last_error"),
            &serde_json::json!("error"),
            None,
        )
        .await
        .unwrap();

        store.clear_current(cwd, session_id).await.unwrap();

        assert!(
            kv.get(&format!(
                "memory/checkpoints/{memory_owner_key}/{project_key}/{session_id}.md"
            ))
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            kv.list_prefix(&format!(
                "memory/status/{memory_owner_key}/{project_key}/{session_id}/"
            ))
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            kv.get(&cleared_session_key(
                &memory_owner_key,
                &project_key,
                session_id,
            ))
            .await
            .unwrap()
            .is_some(),
            "Native tombstone must use the same owner key as MemoryService"
        );
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
            owner_b.load_current("/ains-web").await.unwrap().is_none(),
            "a shared Web backend must not restore another owner's current session"
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
        let loaded = store.load_current("/proj/beta").await.unwrap().unwrap();
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
        let loaded = store.load_current("/proj/gamma").await.unwrap().unwrap();
        // 结构化字段保留
        assert_eq!(loaded.tool_metadata.read_files, vec!["/a.rs".to_string()]);
        // 白名单键保留，非白名单键丢弃
        assert!(loaded.tool_metadata.extra.contains_key("permission_mode"));
        assert!(!loaded.tool_metadata.extra.contains_key("secret_scratch"));
    }

    #[tokio::test]
    async fn save_replaces_the_only_current_snapshot() {
        let kv = kv();
        let store = SessionStore::new(Arc::clone(&kv));
        store
            .save(SessionSaveInput {
                session_id: Some("aaaaaaaaaaaa".into()),
                cwd: "/proj/delta".into(),
                messages: vec![user_msg("goal")],
                ..Default::default()
            })
            .await
            .unwrap();
        let slug = project_slug("/proj/delta");
        kv.set(
            &format!("{}legacy-session", legacy_entry_prefix(&slug)),
            &serde_json::json!("obsolete history entry"),
            None,
        )
        .await
        .unwrap();

        let replacement = store
            .save(SessionSaveInput {
                session_id: Some("bbbbbbbbbbbb".into()),
                cwd: "/proj/delta".into(),
                messages: vec![user_msg("replacement current history")],
                ..Default::default()
            })
            .await
            .unwrap();
        let current = store.load_current("/proj/delta").await.unwrap().unwrap();
        assert_eq!(current.session_id, replacement);
        assert_eq!(
            current.messages,
            vec![user_msg("replacement current history")]
        );
        assert!(
            kv.list_prefix(&legacy_entry_prefix(&slug))
                .await
                .unwrap()
                .is_empty(),
            "a replacement must not create a historical snapshot"
        );
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
        // 回归：同毫秒 + 同 cwd 不能生成相同 id，否则清空屏障与 checkpoint
        // 会把两次运行边界错误地视作同一个会话。
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
}
