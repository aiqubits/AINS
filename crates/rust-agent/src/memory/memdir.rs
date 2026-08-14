//! memdir 可读记忆库（AINS_PLAN Phase 2.8，对齐 Harness `memory/memdir.py` +
//! `memory/schema.py`）。
//!
//! - `MEMORY.md` 为一行式索引，条目正文存放在独立 topic 文件（frontmatter + body）；
//! - 基线以文件系统目录存储，AINS 双端统一落在 KvStore（键前缀 `memdir/`），
//!   team vault / 文件锁 / secret 扫描不在 Phase 2 范围（偏差见对齐文档）；
//! - 时间戳为 ISO-8601 UTC 秒级（`YYYY-MM-DDTHH:MM:SSZ`），手工实现civil 历法换算，
//!   避免引入 chrono 依赖。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::MemoryError;
use crate::memory::kv::{KvStore, now_ms};
use crate::memory::manage::normalize_for_signature;

/// MEMORY.md 载入时的最大行数（基线 `MAX_ENTRYPOINT_LINES`）。
pub const MAX_ENTRYPOINT_LINES: usize = 200;
/// MEMORY.md 载入时的最大字节数（基线 `MAX_ENTRYPOINT_BYTES`）。
pub const MAX_ENTRYPOINT_BYTES: usize = 25_000;
/// frontmatter schema 版本（基线 `SCHEMA_VERSION`）。
pub const SCHEMA_VERSION: u32 = 1;
/// scan 清单最大条目数（基线 `MAX_MANIFEST_FILES`）。
pub const MAX_MANIFEST_FILES: usize = 200;

/// KvStore 内的索引键。
pub const INDEX_KEY: &str = "memdir/MEMORY.md";
/// 条目键前缀。
pub const ENTRY_PREFIX: &str = "memdir/entries/";

/// 持久记忆策略行（逐字对齐基线 `schema.py MEMORY_POLICY_LINES`）。
pub const MEMORY_POLICY_LINES: [&str; 8] = [
    "## Durable memory policy",
    "- Store durable memory only when the information is not cheaply derivable from current files, docs, git history, or tool output.",
    "- Use `type: user|feedback|project|reference` and optional `scope: private|project|team` frontmatter.",
    "- `MEMORY.md` is an index, not a memory body. Keep each pointer one line.",
    "- Update or remove stale contradictions instead of duplicating notes.",
    "- If the user says to ignore memory, proceed as if no memory was loaded and do not cite, apply, or mention memory contents.",
    "- Memory can be stale. Verify remembered project/code state against current files before acting on it.",
    "- Do not save secrets, credentials, private personal context in team memory, or temporary task chatter.",
];

/// 记忆条目类型（基线 `type: user|feedback|project|reference`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    User,
    Feedback,
    #[default]
    Project,
    Reference,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    /// 宽松解析（基线对 note/memory/core/knowledge 等回落默认类型）。
    pub fn parse_lenient(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "user" => Self::User,
            "feedback" => Self::Feedback,
            "reference" => Self::Reference,
            _ => Self::Project,
        }
    }
}

/// 记忆条目可见范围（基线 `scope: private|project|team`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    Private,
    #[default]
    Project,
    Team,
}

impl MemoryScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Project => "project",
            Self::Team => "team",
        }
    }

    /// 宽松解析（personal/user → private，shared → team）。
    pub fn parse_lenient(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "private" | "personal" | "user" => Self::Private,
            "team" | "shared" => Self::Team,
            _ => Self::Project,
        }
    }
}

/// 新建条目输入。
#[derive(Debug, Clone)]
pub struct NewMemoryEntry {
    pub title: String,
    pub body: String,
    pub description: String,
    pub memory_type: MemoryType,
    pub scope: MemoryScope,
    pub importance: f64,
    pub source: String,
    /// 0 表示不过期。
    pub ttl_days: i64,
    pub tags: Vec<String>,
}

impl Default for NewMemoryEntry {
    fn default() -> Self {
        Self {
            title: String::new(),
            body: String::new(),
            description: String::new(),
            memory_type: MemoryType::default(),
            scope: MemoryScope::default(),
            importance: 1.0,
            source: "agent".to_string(),
            ttl_days: 0,
            tags: Vec::new(),
        }
    }
}

/// 已存储条目（scan 输出）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemdirEntry {
    pub filename: String,
    pub id: String,
    pub name: String,
    pub description: String,
    pub memory_type: MemoryType,
    pub scope: MemoryScope,
    pub category: String,
    pub importance: f64,
    pub source: String,
    pub signature: String,
    pub created_at: String,
    pub updated_at: String,
    pub ttl_days: i64,
    pub disabled: bool,
    pub tags: Vec<String>,
    pub body: String,
}

/// memdir 存储（挂在 kv 逻辑表上）。
///
/// 并发契约：复合写操作（[`MemdirStore::add_entry`]、[`MemdirStore::remove_entry`]、
/// [`MemdirStore::clear_entries`]）内部由"读取判定 + 写入 + 索引更新"多步组成，
/// 在 KvStore 层并非原子事务。调用方必须在外部持 `durable_mutation_gate`（或
/// 等价的跨会话串行化）再进入这些方法，否则并发写入同一存储域时去重判定与
/// 索引追加可能交错。
///
/// 锁由宿主层持有：web 端的 `extract_durable_serialized` /
/// `clear_durable_memories` / `delete_durable_memory`（native 持
/// `MemoryService::durable_mutation_gate`，wasm 持 origin 级
/// `DURABLE_MEMORY_WRITE_LOCK`）在调用本 store 或 `MemoryService` 的管理方法
/// 前获取；这些方法自身不获取锁。新增调用方必须沿用同一宿主模式，不能绕过
/// 宿主直接调用。
pub struct MemdirStore {
    kv: Arc<dyn KvStore>,
    /// Web owner partition. Native uses the legacy unscoped keys to retain
    /// existing single-user storage compatibility.
    owner_scope: Option<String>,
}

impl MemdirStore {
    pub fn new(kv: Arc<dyn KvStore>) -> Self {
        Self {
            kv,
            owner_scope: None,
        }
    }

    /// Construct a memdir store whose index and entries are isolated under an
    /// owner-derived, non-empty namespace.
    pub fn new_scoped(kv: Arc<dyn KvStore>, owner_scope: impl Into<String>) -> Self {
        let owner_scope = owner_scope.into();
        Self {
            kv,
            owner_scope: (!owner_scope.trim().is_empty()).then_some(owner_scope),
        }
    }

    fn index_key(&self) -> String {
        match &self.owner_scope {
            Some(owner) => format!("owner/{owner}/{INDEX_KEY}"),
            None => INDEX_KEY.to_string(),
        }
    }

    fn entry_prefix(&self) -> String {
        match &self.owner_scope {
            Some(owner) => format!("owner/{owner}/{ENTRY_PREFIX}"),
            None => ENTRY_PREFIX.to_string(),
        }
    }

    /// 生成注入 system prompt 的 Memory 段（基线 `load_memory_prompt`，恒有输出）。
    pub async fn load_memory_prompt(&self) -> Result<String, MemoryError> {
        let mut lines: Vec<String> = vec![
            "# Memory".to_string(),
            "- Persistent memory directory: kv://memdir".to_string(),
            "- Use this directory to store durable project and repository context that should survive future sessions.".to_string(),
            "- Prefer concise topic files plus an index entry in MEMORY.md.".to_string(),
            String::new(),
        ];
        lines.extend(MEMORY_POLICY_LINES.iter().map(|s| s.to_string()));
        lines.push(String::new());
        lines.push("## MEMORY.md".to_string());

        match self.kv.get(&self.index_key()).await? {
            Some(value) => {
                let raw = value.as_str().unwrap_or_default().to_string();
                let (text, reason) = truncate_entrypoint(&raw);
                let text = match reason {
                    Some(reason) => append_truncation_marker(&text, &reason),
                    None => text,
                };
                if text.trim().is_empty() {
                    lines.push("(not created yet)".to_string());
                } else {
                    lines.push("```md".to_string());
                    lines.push(text.trim_end().to_string());
                    lines.push("```".to_string());
                }
            }
            None => lines.push("(not created yet)".to_string()),
        }
        Ok(lines.join("\n"))
    }

    /// 新增条目：签名去重（重复即刷新），写 topic 文件并追加索引行。
    /// 返回条目文件名（如 `build_setup.md`）。
    pub async fn add_entry(&self, entry: NewMemoryEntry) -> Result<String, MemoryError> {
        let signature = entry_signature(&entry.body, entry.memory_type);
        let now = now_ms();
        let timestamp = format_iso_utc(now);

        // 去重：相同签名 → 刷新既有条目。基线刷新语义（manager.py）：
        // 标题/描述/正文/标签/来源以新写入为准，filename / id /
        // created_at / type / scope / ttl_days 保留既有值。
        for existing in self.scan_raw().await? {
            if existing.signature == signature {
                let mut refreshed = existing.clone();
                refreshed.name = entry.title.clone();
                refreshed.description = entry.description.clone();
                refreshed.body = entry.body.clone();
                refreshed.tags = entry.tags.clone();
                refreshed.source = entry.source.clone();
                refreshed.importance = existing.importance.max(entry.importance).max(1.0);
                refreshed.updated_at = timestamp.clone();
                refreshed.disabled = false;
                self.write_entry(&refreshed).await?;
                // 软删除后重新 add：恢复索引行；标题已变更时同步更新
                // 既有索引行（upsert），避免 MEMORY.md 索引标题陈旧。
                self.upsert_index_line(&refreshed.name, &refreshed.filename)
                    .await?;
                return Ok(existing.filename);
            }
        }

        let filename = self.unique_filename(&entry.title).await?;
        let id = generate_memory_id(&entry.body, now);
        let stored = MemdirEntry {
            filename: filename.clone(),
            id,
            name: entry.title.clone(),
            description: entry.description.clone(),
            memory_type: entry.memory_type,
            scope: entry.scope,
            category: "knowledge".to_string(),
            // 基线两路径均钳位 ≥ 1（schema.py coerce_int 后 max(_, 1)），
            // 与刷新路径口径一致。
            importance: entry.importance.max(1.0),
            source: entry.source.clone(),
            signature,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            ttl_days: entry.ttl_days,
            disabled: false,
            tags: entry.tags.clone(),
            body: entry.body.clone(),
        };
        self.write_entry(&stored).await?;
        self.append_index_line(&entry.title, &filename).await?;
        Ok(filename)
    }

    /// 删除条目（软删除：`disabled: true` 并从索引移除对应行）。
    /// 支持按文件名 / 文件名去后缀 / 标题 / id 匹配；未命中或已禁用返回 false。
    pub async fn remove_entry(&self, query: &str) -> Result<bool, MemoryError> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(false);
        }
        for entry in self.scan_raw().await? {
            let stem = entry.filename.trim_end_matches(".md").to_lowercase();
            let matched = needle == entry.filename.to_lowercase()
                || needle == stem
                || needle == entry.name.to_lowercase()
                || needle == entry.id.to_lowercase();
            if !matched {
                continue;
            }
            if entry.disabled {
                continue;
            }
            let mut updated = entry.clone();
            updated.disabled = true;
            updated.updated_at = format_iso_utc(now_ms());
            self.write_entry(&updated).await?;
            self.drop_index_lines(&entry.filename).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// 永久删除当前存储域内的全部条目及索引，供用户确认的“清空全部”使用。
    /// 返回移除的条目键数（包含损坏条目）。
    pub async fn clear_entries(&self) -> Result<u64, MemoryError> {
        let removed = self.kv.delete_prefix(&self.entry_prefix()).await?;
        self.kv.delete(&self.index_key()).await?;
        Ok(removed)
    }

    /// 永久删除一条条目，供用户主动的数据删除操作使用。
    pub async fn delete_entry(&self, query: &str) -> Result<bool, MemoryError> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(false);
        }
        for entry in self.scan_raw().await? {
            let stem = entry.filename.trim_end_matches(".md").to_lowercase();
            if needle != entry.filename.to_lowercase()
                && needle != stem
                && needle != entry.name.to_lowercase()
                && needle != entry.id.to_lowercase()
            {
                continue;
            }
            self.kv
                .delete(&format!("{}{}", self.entry_prefix(), entry.filename))
                .await?;
            self.drop_index_lines(&entry.filename).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Permanently delete exactly one entry identified by its canonical id.
    ///
    /// Management UIs already retain this id, so they must not use the
    /// user-friendly `delete_entry` matcher: a user-controlled title can
    /// otherwise equal another entry's id and remove a different card.
    pub async fn delete_entry_by_id(&self, id: &str) -> Result<bool, MemoryError> {
        let id = id.trim();
        if id.is_empty() {
            return Ok(false);
        }
        for entry in self.scan_raw().await? {
            if entry.id != id {
                continue;
            }
            self.kv
                .delete(&format!("{}{}", self.entry_prefix(), entry.filename))
                .await?;
            self.drop_index_lines(&entry.filename).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// 扫描有效条目：过滤 disabled / TTL 过期，按 updated_at 降序，截断 max_files。
    pub async fn scan(&self, max_files: usize) -> Result<Vec<MemdirEntry>, MemoryError> {
        let now = now_ms();
        let mut entries: Vec<MemdirEntry> = self
            .scan_raw()
            .await?
            .into_iter()
            .filter(|e| !e.disabled && !is_ttl_expired(e, now))
            .collect();
        // ISO-8601 UTC 字符串可直接按字典序比较时间先后。
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        entries.truncate(max_files.min(MAX_MANIFEST_FILES));
        Ok(entries)
    }

    /// 读取原始索引文本（不存在返回 None）。
    pub async fn read_index(&self) -> Result<Option<String>, MemoryError> {
        Ok(self
            .kv
            .get(&self.index_key())
            .await?
            .and_then(|v| v.as_str().map(|s| s.to_string())))
    }

    async fn scan_raw(&self) -> Result<Vec<MemdirEntry>, MemoryError> {
        let entry_prefix = self.entry_prefix();
        let keys = self.kv.list_prefix(&entry_prefix).await?;
        let mut entries = Vec::with_capacity(keys.len());
        for key in keys {
            // 单行损坏（JSON 载荷无法解码）跳过，不毒化整个 memdir；
            // 其余存储错误照常上抛。
            let value = match self.kv.get(&key).await {
                Ok(Some(value)) => value,
                Ok(None) => continue,
                Err(MemoryError::Serialization(e)) => {
                    tracing::warn!(key, error = %e, "skipping corrupt memdir row");
                    continue;
                }
                Err(e) => return Err(e),
            };
            let Some(raw) = value.as_str() else { continue };
            let filename = key.trim_start_matches(&entry_prefix).to_string();
            if let Some(entry) = parse_entry_file(&filename, raw) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    async fn write_entry(&self, entry: &MemdirEntry) -> Result<(), MemoryError> {
        let file = render_entry_file(entry);
        let key = format!("{}{}", self.entry_prefix(), entry.filename);
        self.kv.set(&key, &Value::String(file), None).await
    }

    async fn unique_filename(&self, title: &str) -> Result<String, MemoryError> {
        let slug = slugify(title);
        // `list_prefix` 是原始前缀扫描，包含损坏行在内，因此先按占用集合
        // 去重；若候选名已被一条损坏行占用（载荷不可解析），其文件名视为
        // 可复用——`write_entry` 会覆写该键，从而自愈损坏行（其内容本就
        // 不可读，无信息可保留）。
        let entry_prefix = self.entry_prefix();
        let existing = self.kv.list_prefix(&entry_prefix).await?;
        let taken: Vec<&str> = existing
            .iter()
            .map(|k| k.trim_start_matches(&entry_prefix))
            .collect();
        let mut candidate = format!("{slug}.md");
        let mut i = 2;
        loop {
            if !taken.contains(&candidate.as_str()) {
                return Ok(candidate);
            }
            if self
                .row_is_corrupt(&format!("{entry_prefix}{candidate}"))
                .await?
            {
                return Ok(candidate);
            }
            candidate = format!("{slug}_{i}.md");
            i += 1;
        }
    }

    /// 判断某键是否为一条“损坏行”：载荷无法解码（KV Serialization 错误）
    /// 或解码后不是字符串。与 `scan_raw` 对损坏行的判定口径保持一致。
    async fn row_is_corrupt(&self, key: &str) -> Result<bool, MemoryError> {
        match self.kv.get(key).await {
            Ok(Some(value)) => Ok(value.as_str().is_none()),
            // 键在 list 与 get 之间消失：视为可复用。
            Ok(None) => Ok(true),
            Err(MemoryError::Serialization(_)) => Ok(true),
            Err(e) => Err(e),
        }
    }

    async fn append_index_line(&self, title: &str, filename: &str) -> Result<(), MemoryError> {
        let mut index = self.read_index().await?.unwrap_or_default();
        // 锚定行尾 `]({filename})`：索引行固定为 `- [title](file)` 形态，
        // 全文 contains 判定会被标题文本里出现的 `(build.md)` 字样误伤
        // （误判已存在而漏追加 / 误删无关条目的索引行）。
        let anchor = format!("]({filename})");
        if index.lines().any(|line| line.trim_end().ends_with(&anchor)) {
            return Ok(());
        }
        if !index.is_empty() && !index.ends_with('\n') {
            index.push('\n');
        }
        // 与 `upsert_index_line` 的刷新路径一致：标题含换行/`]` 会破坏
        // 索引行的 markdown 链接结构，写前同样清洗（review P3）。
        let safe_title = sanitize_index_text(title);
        index.push_str(&format!("- [{safe_title}]({filename})\n"));
        self.kv
            .set(&self.index_key(), &Value::String(index), None)
            .await
    }

    /// 刷新/更新既有条目的索引行：锚定 `]({filename})` 的行存在则整行
    /// 重写为新标题，否则按 `append_index_line` 语义追加。刷新路径复用
    /// append 会因锚定行已存在而跳过，导致索引标题与条目 frontmatter
    /// 的 `name` 不一致（陈旧标题会误导模型）。
    async fn upsert_index_line(&self, title: &str, filename: &str) -> Result<(), MemoryError> {
        let index = self.read_index().await?.unwrap_or_default();
        let anchor = format!("]({filename})");
        // 标题含换行/`]` 会破坏索引行的 markdown 链接结构，写前清洗。
        let safe_title = sanitize_index_text(title);
        // 锚定行存在：整行重写。用 `lines()` 逐行重建，避免 `split('\n')`
        // 在结尾换行时多出一个空串导致的多余空行。
        if index.lines().any(|line| line.trim_end().ends_with(&anchor)) {
            let mut text = String::new();
            for line in index.lines() {
                if line.trim_end().ends_with(&anchor) {
                    text.push_str(&format!("- [{safe_title}]({filename})"));
                } else {
                    // lines() 对 CRLF 保留 \r：整文件重建时剥掉，避免重写后
                    // 混合行尾（锚定行输出 LF、其余行残留 CRLF）。
                    text.push_str(line.strip_suffix('\r').unwrap_or(line));
                }
                text.push('\n');
            }
            return self
                .kv
                .set(&self.index_key(), &Value::String(text), None)
                .await;
        }
        // 无既有行：追加（保持与 append_index_line 一致的换行语义）。
        let mut index = index;
        if !index.is_empty() && !index.ends_with('\n') {
            index.push('\n');
        }
        index.push_str(&format!("- [{safe_title}]({filename})\n"));
        self.kv
            .set(&self.index_key(), &Value::String(index), None)
            .await
    }

    async fn drop_index_lines(&self, filename: &str) -> Result<(), MemoryError> {
        let Some(index) = self.read_index().await? else {
            return Ok(());
        };
        // 与 append_index_line 同口径：只删行尾锚定命中的索引行
        let anchor = format!("]({filename})");
        let kept: Vec<&str> = index
            .lines()
            .filter(|line| !line.trim_end().ends_with(&anchor))
            .collect();
        let mut text = kept.join("\n");
        if index.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        self.kv
            .set(&self.index_key(), &Value::String(text), None)
            .await
    }
}

/// 条目签名：`sha256("{normalized_body}|{type}|knowledge")`（对齐基线签名口径）。
pub fn entry_signature(body: &str, memory_type: MemoryType) -> String {
    let normalized = normalize_for_signature(body);
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hasher.update(b"|");
    hasher.update(memory_type.as_str().as_bytes());
    hasher.update(b"|knowledge");
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 记忆 id：`mem-{紧凑时间戳}-{8 位 hex}`（基线 `generate_memory_id`；
/// 随机 hex 以内容+时间的确定性哈希替代，保证双端可复现）。
pub fn generate_memory_id(content: &str, now_ms: i64) -> String {
    let iso = format_iso_utc(now_ms);
    let compact = iso
        .replace(['-', ':'], "")
        .replace('T', "-")
        .replace('Z', "");
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher.update(b"|");
    hasher.update(now_ms.to_le_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("mem-{compact}-{hex}")
}

/// 索引行标题清洗：去除控制字符并剥掉 `[`/`]`，防止破坏
/// `- [title](file)` 的 markdown 链接结构。
pub fn sanitize_index_text(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_control() && *c != '[' && *c != ']')
        .collect()
}

/// 标题 → slug：非字母数字折叠为 `_`，小写，去首尾 `_`，空回落 `memory`。
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut last_was_sep = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep && !slug.is_empty() {
            slug.push('_');
            last_was_sep = true;
        }
    }
    let slug = slug.trim_matches('_').to_string();
    if slug.is_empty() {
        "memory".to_string()
    } else {
        slug
    }
}

/// MEMORY.md 截断：先按行截，再按字节截（回退到最后一个完整行），
/// 字节超限原因优先；返回（截断后文本，Some(原因)）。
pub fn truncate_entrypoint(raw: &str) -> (String, Option<String>) {
    let raw_bytes = raw.len();
    let line_count = raw.lines().count();
    let mut text = raw.to_string();
    let mut reason: Option<String> = None;

    if line_count > MAX_ENTRYPOINT_LINES {
        let kept: Vec<&str> = raw.lines().take(MAX_ENTRYPOINT_LINES).collect();
        text = kept.join("\n");
        reason = Some(format!(
            "{line_count} lines (limit: {MAX_ENTRYPOINT_LINES})"
        ));
    }

    if text.len() > MAX_ENTRYPOINT_BYTES {
        let slice = &text.as_bytes()[..MAX_ENTRYPOINT_BYTES];
        let valid = match std::str::from_utf8(slice) {
            Ok(s) => s,
            Err(e) => std::str::from_utf8(&slice[..e.valid_up_to()]).unwrap_or_default(),
        };
        let mut cut = valid.to_string();
        if let Some(pos) = cut.rfind('\n')
            && pos > 0
        {
            cut.truncate(pos);
        }
        text = cut;
        reason = Some(format!("{raw_bytes} bytes (limit: {MAX_ENTRYPOINT_BYTES})"));
    }

    if reason.is_some() && raw.ends_with('\n') && !text.ends_with('\n') {
        text.push('\n');
    }
    (text, reason)
}

/// 截断警示标记（逐字对齐基线 marker 文案）。
pub fn append_truncation_marker(text: &str, reason: &str) -> String {
    format!(
        "{}\n\n> WARNING: MEMORY.md is {reason}. Only part of it was loaded. Keep index entries one line and move detail into topic notes.\n",
        text.trim_end()
    )
}

/// frontmatter 字段渲染顺序（基线 `FRONTMATTER_FIELDS`）。
fn render_entry_file(entry: &MemdirEntry) -> String {
    let mut fm = String::new();
    fm.push_str(&format!("schema_version: {SCHEMA_VERSION}\n"));
    fm.push_str(&format!("id: {}\n", entry.id));
    fm.push_str(&format!("name: {}\n", yaml_quote(&entry.name)));
    fm.push_str(&format!(
        "description: {}\n",
        yaml_quote(&entry.description)
    ));
    fm.push_str(&format!("type: {}\n", entry.memory_type.as_str()));
    fm.push_str(&format!("scope: {}\n", entry.scope.as_str()));
    fm.push_str(&format!("category: {}\n", entry.category));
    fm.push_str(&format!("importance: {}\n", entry.importance));
    fm.push_str(&format!("source: {}\n", yaml_quote(&entry.source)));
    fm.push_str(&format!("signature: {}\n", entry.signature));
    fm.push_str(&format!("created_at: {}\n", entry.created_at));
    fm.push_str(&format!("updated_at: {}\n", entry.updated_at));
    fm.push_str(&format!("ttl_days: {}\n", entry.ttl_days));
    fm.push_str(&format!("disabled: {}\n", entry.disabled));
    if !entry.tags.is_empty() {
        fm.push_str("tags:\n");
        for tag in &entry.tags {
            fm.push_str(&format!("  - {}\n", yaml_quote(tag)));
        }
    }
    format!("---\n{fm}---\n\n{}", entry.body)
}

/// YAML 双引号风格转义（`{:?}` 的 Rust 转义与 YAML 不兼容，需手写）。
fn yaml_quote(text: &str) -> String {
    let needs_quote = text.is_empty()
        || text.contains([':', '#', '"', '\\'])
        || text.chars().any(|c| (c as u32) < 0x20)
        || text.starts_with(['-', '[', '{', '&', '*', '!', '|', '>', '\'', '%', '@', '?'])
        || text.starts_with(' ')
        || text.ends_with(' ')
        || is_yaml_native_scalar(text);
    if !needs_quote {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 未加引号时会被 YAML 解析为 bool / null / number 的字面形态，
/// 必须加引号才能 roundtrip 为字符串。
fn is_yaml_native_scalar(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~"
    ) || text.parse::<f64>().is_ok()
}

/// 解析条目文件：宽松 frontmatter（serde_yaml），缺失字段回落默认值。
fn parse_entry_file(filename: &str, raw: &str) -> Option<MemdirEntry> {
    let (frontmatter, body) = split_frontmatter(raw);
    let map: serde_yaml::Value = frontmatter
        .and_then(|fm| serde_yaml::from_str(fm).ok())
        .unwrap_or(serde_yaml::Value::Null);

    let get_str = |key: &str| -> String {
        map.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let description = {
        let d = get_str("description");
        if d.is_empty() {
            fallback_description(body)
        } else {
            d
        }
    };

    Some(MemdirEntry {
        filename: filename.to_string(),
        id: get_str("id"),
        name: {
            let n = get_str("name");
            if n.is_empty() {
                filename.trim_end_matches(".md").to_string()
            } else {
                n
            }
        },
        description,
        memory_type: MemoryType::parse_lenient(&get_str("type")),
        scope: MemoryScope::parse_lenient(&get_str("scope")),
        category: {
            let c = get_str("category");
            if c.is_empty() {
                "knowledge".to_string()
            } else {
                c
            }
        },
        importance: map
            .get("importance")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0),
        source: get_str("source"),
        signature: get_str("signature"),
        created_at: get_str("created_at"),
        updated_at: get_str("updated_at"),
        ttl_days: map.get("ttl_days").and_then(|v| v.as_i64()).unwrap_or(0),
        disabled: map
            .get("disabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        tags: map
            .get("tags")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        body: body.to_string(),
    })
}

fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return (None, raw);
    };
    // 以行首 `---` 结束 frontmatter，避免字段值内含 `---` 时提前截断。
    match rest.split_once("\n---") {
        Some((fm, body)) => (Some(fm), body.trim_start_matches('\n')),
        None => (None, raw),
    }
}

/// 描述回落：正文中第一个非 `#` / 非 `---` 的非空行，截断到 200 字符。
fn fallback_description(body: &str) -> String {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("---") {
            continue;
        }
        return trimmed.chars().take(200).collect();
    }
    String::new()
}

fn is_ttl_expired(entry: &MemdirEntry, now_ms: i64) -> bool {
    if entry.ttl_days <= 0 {
        return false;
    }
    // 基线 schema.py：TTL 以最后更新时间为锚（去重刷新延长寿命），
    // updated_at 缺失/非法时回落 created_at。
    let Some(base_ms) =
        parse_iso_utc(&entry.updated_at).or_else(|| parse_iso_utc(&entry.created_at))
    else {
        return false;
    };
    // ttl_days 来自模型输出（不可信）：饱和乘加，超大值等效永不过期。
    // 边界取 >=（恰好到期即过期），同基线 `now >= base + ttl`。
    let ttl_ms = entry.ttl_days.saturating_mul(24 * 3600 * 1000);
    now_ms >= base_ms.saturating_add(ttl_ms)
}

// ── ISO-8601 UTC 秒级时间戳（Howard Hinnant civil 历法算法，双端纯 Rust）──

/// epoch 毫秒 → `YYYY-MM-DDTHH:MM:SSZ`。
pub fn format_iso_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// `YYYY-MM-DDTHH:MM:SSZ` → epoch 毫秒（格式非法返回 None）。
pub fn parse_iso_utc(text: &str) -> Option<i64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year_text = date_parts.next()?;
    let month_text = date_parts.next()?;
    let day_text = date_parts.next()?;
    if date_parts.next().is_some()
        || !is_fixed_ascii_digits(year_text, 4)
        || !is_fixed_ascii_digits(month_text, 2)
        || !is_fixed_ascii_digits(day_text, 2)
    {
        return None;
    }
    let y: i64 = year_text.parse().ok()?;
    let m: u32 = month_text.parse().ok()?;
    let d: u32 = day_text.parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    let max_day = days_in_month(y, m);
    if !(1..=max_day).contains(&d) {
        return None;
    }
    let mut time_parts = time.split(':');
    let hour_text = time_parts.next()?;
    let minute_text = time_parts.next()?;
    let second_text = time_parts.next()?;
    if time_parts.next().is_some()
        || !is_fixed_ascii_digits(hour_text, 2)
        || !is_fixed_ascii_digits(minute_text, 2)
    {
        return None;
    }
    let h: i64 = hour_text.parse().ok()?;
    let mi: i64 = minute_text.parse().ok()?;
    let (whole_second_text, fraction_text) = match second_text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (second_text, None),
    };
    if !is_fixed_ascii_digits(whole_second_text, 2)
        || fraction_text.is_some_and(|fraction| {
            fraction.is_empty()
                || fraction.len() > 3
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&mi)
    {
        return None;
    }
    let s: i64 = whole_second_text.parse().ok()?;
    if !(0..=59).contains(&s) {
        return None;
    }
    let fractional_ms = match fraction_text {
        Some(fraction) => fraction.parse::<i64>().ok()? * 10i64.pow(3 - fraction.len() as u32),
        None => 0,
    };
    let days = days_from_civil(y, m, d);
    let whole_seconds = days
        .checked_mul(86_400)?
        .checked_add(h.checked_mul(3600)?)?
        .checked_add(mi.checked_mul(60)?)?
        .checked_add(s)?;
    whole_seconds.checked_mul(1000)?.checked_add(fractional_ms)
}

fn is_fixed_ascii_digits(text: &str, length: usize) -> bool {
    text.len() == length && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.rem_euclid(400) == 0
            || (year.rem_euclid(4) == 0 && year.rem_euclid(100) != 0) =>
        {
            29
        }
        2 => 28,
        _ => 0,
    }
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::{format_iso_utc, parse_iso_utc, slugify};

    /// 手写历法换算的已知值对照（闰年/世纪年/负时间戳边界）。
    /// 间接路径（TTL 相对差值）中换算错误会自行抵消，必须直接对拍。
    #[test]
    fn iso_utc_known_values() {
        assert_eq!(format_iso_utc(0), "1970-01-01T00:00:00Z");
        // 2000 是闰年（能被 400 整除）
        assert_eq!(format_iso_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        // 2020 闰年带时分秒
        assert_eq!(format_iso_utc(1_582_979_696_000), "2020-02-29T12:34:56Z");
        // 2100 不是闰年（能被 100 整除但不能被 400 整除）
        assert_eq!(format_iso_utc(4_102_444_800_000), "2100-01-01T00:00:00Z");
        // epoch 前：div_euclid 负方向取整
        assert_eq!(format_iso_utc(-1_000), "1969-12-31T23:59:59Z");

        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00.123Z"), Some(123));
        assert_eq!(parse_iso_utc("2000-02-29T00:00:00Z"), Some(951_782_400_000));
    }

    #[test]
    fn iso_utc_roundtrip() {
        // 月末/年末/闰日/世纪边界；毫秒取整秒（格式为秒级）
        for ms in [
            0i64,
            86_399_000,
            951_782_400_000,
            1_582_979_696_000,
            4_102_444_800_000,
            253_402_300_799_000, // 9999-12-31T23:59:59Z
        ] {
            let text = format_iso_utc(ms);
            assert_eq!(
                parse_iso_utc(&text),
                Some(ms),
                "roundtrip failed for {text}"
            );
        }
    }

    #[test]
    fn iso_utc_rejects_malformed() {
        for bad in [
            "",
            "2020-02-29",
            "2020-02-29 12:34:56Z",
            "2020-02-29T12:34:56",
            "2021-02-29T12:34:56Z",
            "2020-00-01T00:00:00Z",
            "2020-13-01T00:00:00Z",
            "2020-01-00T00:00:00Z",
            "2020-01-01T24:00:00Z",
            "2020-01-01T00:60:00Z",
            "2020-01-01T00:00:60Z",
            "20-01-01T00:00:00Z",
            "2020-1-01T00:00:00Z",
            "2020-01-01T0:00:00Z",
            "2020-01-01T00:00:1e1Z",
            "2020-01-01T00:00:00.1234Z",
            "2020-01-01T00:00:00:extraZ",
            "10000-01-01T00:00:00Z",
            "not-a-date",
        ] {
            assert_eq!(parse_iso_utc(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn slugify_non_ascii_falls_back() {
        assert_eq!(slugify("Build Setup"), "build_setup");
        // 纯非 ASCII 标题回落固定 slug（冲突由 unique_filename 的 _2 后缀化解）
        assert_eq!(slugify("构建配置"), "memory");
        assert_eq!(slugify("  ---  "), "memory");
    }

    // 双端可跑：native 用 tokio，wasm 用 wasm-bindgen-test（无 tokio 运行时）。
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    async fn upsert_index_line_normalizes_crlf_on_rewrite() {
        use crate::memory::in_memory::InMemoryKvStore;
        use crate::memory::kv::KvStore;
        use serde_json::Value;
        use std::sync::Arc;

        let kv: Arc<dyn KvStore> = Arc::new(InMemoryKvStore::default());
        let store = super::MemdirStore::new(Arc::clone(&kv));
        // 既有 CRLF 索引（模拟跨平台写入的历史文件）：两行都以 \r\n 结尾，
        // 其中第一行锚定 build.md。
        kv.set(
            &store.index_key(),
            &Value::String("- [Old title](build.md)\r\n- [Another](other.md)\r\n".into()),
            None,
        )
        .await
        .unwrap();
        store
            .upsert_index_line("New title", "build.md")
            .await
            .unwrap();
        let rewritten = kv
            .get(&store.index_key())
            .await
            .unwrap()
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap();
        // 整文件重建后统一 LF：锚定行重写为 LF，其余行剥掉残留 \r。
        assert_eq!(
            rewritten,
            "- [New title](build.md)\n- [Another](other.md)\n"
        );
    }
}
