//! KvStore 后端的 Skills 存储（AINS_PLAN 第六章 6.1 存储节，Phase 6.4 最小子集）。
//!
//! key 空间：
//! - `skills:{name}`      → SKILL.md 原始文本（YAML frontmatter + Markdown body）
//! - `skills_meta:{name}` → [`SkillMeta`]（JSON 载荷；KvStore 信封本身已 bincode 落盘，
//!   与计划"bincode(SkillMeta)"的偏差记录见 Phase 6 对齐清单）
//!
//! 本阶段仅实现管理面板所需的 `list` / `load` / `delete_skill` 与内容
//! checksum 校验；`create/update/rollback_skill`、Level 2 引用文件与完整
//! 门控随 Phase 6.8/6.9 落地（调用返回显式错误，不静默成功）。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{MemoryError, SkillsError};
use crate::memory::{KvStore, now_ms};
use crate::platform::Platform;
use crate::skills::{SkillContent, SkillContext, SkillLoader, SkillManage, SkillSummary};

/// SKILL.md 原文的 key 前缀。
pub const SKILL_KEY_PREFIX: &str = "skills:";
/// 元数据的 key 前缀（与 `skills:` 无前缀包含关系，list_prefix 不互扰）。
pub const SKILL_META_KEY_PREFIX: &str = "skills_meta:";
/// 单个 SKILL.md 原文的字节上限（防超大条目撞爆面板/上下文）。
pub const MAX_SKILL_CONTENT_BYTES: usize = 256 * 1024;

/// 版本记录 key 前缀：`skills_ver:{name}:{version}`。
pub const SKILL_VER_KEY_PREFIX: &str = "skills_ver:";
/// 技能头（活跃版本指针 + 元数据）key 前缀：`skills_head:{name}`。
pub const SKILL_HEAD_KEY_PREFIX: &str = "skills_head:";
/// Level 2 引用文件 key 前缀：`skills_ref:{name}:{path}`。
pub const SKILL_REF_KEY_PREFIX: &str = "skills_ref:";
/// 默认保留最近版本数（第六章清理策略：最近 3 + 1 Golden）。
pub const DEFAULT_MAX_RETAINED_VERSIONS: usize = 3;
/// 自动回滚阈值：当前版本连续失败次数（第六章回滚条件）。
pub const AUTO_ROLLBACK_CONSECUTIVE_FAILURES: u32 = 5;

/// Skill 生命周期状态（第六章 Skill Lifecycle）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillStatus {
    /// 活跃可用。
    Active,
    /// 已弃用（被新版本或回滚取代）。
    Deprecated,
    /// 已过期。
    Expired,
    /// 被撤销。
    Revoked,
}

/// Skill 版本评分（第六章回滚/清理依据）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SkillScore {
    pub successes: u32,
    pub failures: u32,
    /// 连续失败次数（成功则归零），驱动自动回滚。
    pub consecutive_failures: u32,
}

impl SkillScore {
    /// 成功率（无样本时为 0）。
    pub fn success_rate(&self) -> f32 {
        let total = self.successes + self.failures;
        if total == 0 {
            0.0
        } else {
            self.successes as f32 / total as f32
        }
    }

    /// 记录一次执行结果。
    pub fn record(&mut self, ok: bool) {
        if ok {
            self.successes += 1;
            self.consecutive_failures = 0;
        } else {
            self.failures += 1;
            self.consecutive_failures += 1;
        }
    }
}

/// 版本号 `v{major}.{minor}`（排序用：先比 major 再比 minor）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SkillVersion {
    pub major: u32,
    pub minor: u32,
}

impl SkillVersion {
    pub const INITIAL: Self = Self { major: 1, minor: 0 };

    pub fn parse(s: &str) -> Option<Self> {
        let body = s.strip_prefix('v')?;
        let (major, minor) = body.split_once('.')?;
        Some(Self {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    }

    pub fn label(&self) -> String {
        format!("v{}.{}", self.major, self.minor)
    }

    /// 小版本递增（流程微调）。
    pub fn next_minor(&self) -> Self {
        Self {
            major: self.major,
            minor: self.minor + 1,
        }
    }

    /// 大版本递增（流程重构/回滚）。
    pub fn next_major(&self) -> Self {
        Self {
            major: self.major + 1,
            minor: 0,
        }
    }
}

/// 单个版本记录（内容 + 状态 + 评分）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionRecord {
    pub content: String,
    pub checksum: String,
    pub status: SkillStatus,
    pub score: SkillScore,
    pub created_at: i64,
}

/// 技能头：活跃版本指针 + 技能级元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillHead {
    /// 当前活跃版本号（如 `v2.0`）。
    pub active: String,
    pub meta: SkillMeta,
}

/// Skill 信任级别（第六章 6.1；只有 Agent/系统来源，无用户导入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillTrust {
    /// 系统内置 Skill，无限制执行。
    System,
    /// 经验证的信任 Skill，自动执行。
    Trusted,
    /// Agent 生成的 Skill，默认隔离。
    Generated,
    /// 临时 Skill，单次任务有效。
    Temporary,
}

/// Skill 元数据（第六章 6.1 定义 + `description` 冗余字段：
/// 列表渲染免于逐条解析 SKILL.md 全文）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub description: String,
    pub category: String,
    pub requires_tools: Vec<String>,
    /// 适用平台；空表示全平台。
    pub platforms: Vec<Platform>,
    pub trust_level: SkillTrust,
    /// 创建者标识（"agent" | "system"）。
    pub creator: String,
    /// Unix 毫秒。
    pub created_at: i64,
    /// 权限声明列表。
    pub permissions: Vec<String>,
    /// SKILL.md 原文的 sha256（hex），list 时校验完整性。
    pub checksum: String,
}

/// 面板列表条目：元数据可用性与损坏标记（损坏条目标记而非静默跳过，
/// 面板仅允许删除）。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    /// 元数据缺失/反序列化失败/checksum 不匹配时为 `None`。
    pub meta: Option<SkillMeta>,
    pub corrupted: bool,
}

/// 计算 SKILL.md 原文的 sha256 hex。
pub fn skill_checksum(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 拆分 SKILL.md 为（YAML frontmatter 原文，body）。无 frontmatter 时
/// frontmatter 为空串、body 为全文。
pub fn split_frontmatter(raw: &str) -> (String, String) {
    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (String::new(), raw.to_string());
    };
    // 关闭栏必须独占一行（"\n---\n" / "\n---" 结尾）
    for (idx, _) in rest.match_indices("---") {
        let line_start = idx == 0 || rest.as_bytes().get(idx.wrapping_sub(1)) == Some(&b'\n');
        if !line_start {
            continue;
        }
        let after = &rest[idx + 3..];
        if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
            let frontmatter = rest[..idx].trim_end().to_string();
            let body = after.trim_start_matches(['\r', '\n']).to_string();
            return (frontmatter, body);
        }
    }
    // 未闭合：视为无 frontmatter
    (String::new(), raw.to_string())
}

/// KvStore 后端的 Skill 存储（Native redb / Web IndexedDB 复用同一实现）。
pub struct KvSkillStore {
    kv: Arc<dyn KvStore>,
    /// 版本保留窗口（最近 N）：回滚候选与 [`SkillPruner`] 清理共用
    /// 同一权威值，避免“清理保留但回滚拒用”的窗口不一致。
    max_retained_versions: usize,
}

impl KvSkillStore {
    pub fn new(kv: Arc<dyn KvStore>) -> Self {
        Self {
            kv,
            max_retained_versions: DEFAULT_MAX_RETAINED_VERSIONS,
        }
    }

    /// 自定义保留窗口（至少 1；回滚与清理共用）。
    pub fn with_retention(kv: Arc<dyn KvStore>, max_retained_versions: usize) -> Self {
        Self {
            kv,
            max_retained_versions: max_retained_versions.max(1),
        }
    }

    fn content_key(name: &str) -> String {
        format!("{SKILL_KEY_PREFIX}{name}")
    }

    fn meta_key(name: &str) -> String {
        format!("{SKILL_META_KEY_PREFIX}{name}")
    }

    /// 校验 skill 名称：非空、无路径分隔符/控制字符（key 注入防护）。
    fn validate_name(name: &str) -> Result<(), SkillsError> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed != name {
            return Err(SkillsError::InvalidFormat(
                "skill name must be non-empty without surrounding whitespace".into(),
            ));
        }
        if name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        {
            return Err(SkillsError::InvalidFormat(format!(
                "skill name contains forbidden characters: {name}"
            )));
        }
        Ok(())
    }

    /// 低层写入：原文 + 元数据双 key。checksum 由本方法计算（调用方给出的
    /// meta.checksum 被覆盖），供测试注入与 Phase 6.8 `create_skill` 复用。
    pub async fn put_skill(
        &self,
        name: &str,
        content: &str,
        mut meta: SkillMeta,
    ) -> Result<(), SkillsError> {
        Self::validate_name(name)?;
        if content.len() > MAX_SKILL_CONTENT_BYTES {
            return Err(SkillsError::InvalidFormat(format!(
                "skill content exceeds {MAX_SKILL_CONTENT_BYTES} bytes"
            )));
        }
        meta.checksum = skill_checksum(content);
        let meta_value = serde_json::to_value(&meta)
            .map_err(|e| SkillsError::Storage(format!("meta serialization: {e}")))?;
        self.kv
            .set(
                &Self::content_key(name),
                &Value::String(content.into()),
                None,
            )
            .await
            .map_err(storage_err)?;
        self.kv
            .set(&Self::meta_key(name), &meta_value, None)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn raw_content(&self, name: &str) -> Result<Option<String>, SkillsError> {
        match self
            .kv
            .get(&Self::content_key(name))
            .await
            .map_err(storage_err)?
        {
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(SkillsError::Storage(format!(
                "skill content for `{name}` has non-string payload"
            ))),
            None => Ok(None),
        }
    }

    // ── 版本化存储（Phase 6.8/6.9） ─────────────────────────────────────

    fn head_key(name: &str) -> String {
        format!("{SKILL_HEAD_KEY_PREFIX}{name}")
    }

    fn ver_prefix(name: &str) -> String {
        format!("{SKILL_VER_KEY_PREFIX}{name}:")
    }

    fn version_key(name: &str, version: &str) -> String {
        format!("{SKILL_VER_KEY_PREFIX}{name}:{version}")
    }

    fn ref_prefix(name: &str) -> String {
        format!("{SKILL_REF_KEY_PREFIX}{name}:")
    }

    fn ref_key(name: &str, path: &str) -> String {
        format!("{SKILL_REF_KEY_PREFIX}{name}:{path}")
    }

    /// 从 SKILL.md frontmatter 提取元数据（Agent 创建时自描述）。
    /// 缺失字段用默认值；trust 固定 Generated、creator=agent。
    fn meta_from_content(content: &str, now: i64) -> SkillMeta {
        #[derive(Deserialize, Default)]
        struct Fm {
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            category: Option<String>,
            #[serde(default)]
            requires_tools: Option<Vec<String>>,
            #[serde(default, rename = "allowed-tools", alias = "allowed_tools")]
            allowed_tools: Option<Vec<String>>,
            #[serde(default)]
            platforms: Option<Vec<Platform>>,
            #[serde(default)]
            permissions: Option<Vec<String>>,
        }
        let (fm_raw, _body) = split_frontmatter(content);
        let fm: Fm = if fm_raw.is_empty() {
            Fm::default()
        } else {
            serde_yaml::from_str(&fm_raw).unwrap_or_default()
        };
        SkillMeta {
            description: fm.description.unwrap_or_default(),
            category: fm.category.unwrap_or_else(|| "general".into()),
            requires_tools: fm.requires_tools.or(fm.allowed_tools).unwrap_or_default(),
            platforms: fm.platforms.unwrap_or_default(),
            trust_level: SkillTrust::Generated,
            creator: "agent".into(),
            created_at: now,
            permissions: fm.permissions.unwrap_or_default(),
            checksum: skill_checksum(content),
        }
    }

    async fn read_head(&self, name: &str) -> Result<Option<SkillHead>, SkillsError> {
        match self
            .kv
            .get(&Self::head_key(name))
            .await
            .map_err(storage_err)?
        {
            Some(value) => serde_json::from_value(value)
                .map(Some)
                .map_err(|e| SkillsError::Storage(format!("head deser: {e}"))),
            None => Ok(None),
        }
    }

    async fn write_head(&self, name: &str, head: &SkillHead) -> Result<(), SkillsError> {
        let value = serde_json::to_value(head)
            .map_err(|e| SkillsError::Storage(format!("head ser: {e}")))?;
        self.kv
            .set(&Self::head_key(name), &value, None)
            .await
            .map_err(storage_err)
    }

    async fn read_version(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<VersionRecord>, SkillsError> {
        match self
            .kv
            .get(&Self::version_key(name, version))
            .await
            .map_err(storage_err)?
        {
            Some(value) => serde_json::from_value(value)
                .map(Some)
                .map_err(|e| SkillsError::Storage(format!("version deser: {e}"))),
            None => Ok(None),
        }
    }

    async fn write_version(
        &self,
        name: &str,
        version: &str,
        record: &VersionRecord,
    ) -> Result<(), SkillsError> {
        let value = serde_json::to_value(record)
            .map_err(|e| SkillsError::Storage(format!("version ser: {e}")))?;
        self.kv
            .set(&Self::version_key(name, version), &value, None)
            .await
            .map_err(storage_err)
    }

    /// 列出技能全部版本（按版本号升序；无法解析的版本号跳过）。
    pub async fn list_versions(
        &self,
        name: &str,
    ) -> Result<Vec<(SkillVersion, VersionRecord)>, SkillsError> {
        let prefix = Self::ver_prefix(name);
        let keys = self.kv.list_prefix(&prefix).await.map_err(storage_err)?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(ver_str) = key.strip_prefix(&prefix) else {
                continue;
            };
            let Some(ver) = SkillVersion::parse(ver_str) else {
                continue;
            };
            if let Some(rec) = self.read_version(name, ver_str).await? {
                out.push((ver, rec));
            }
        }
        out.sort_by_key(|(ver, _)| *ver);
        Ok(out)
    }

    /// 保留集：最近 N 个版本（窗口见 `max_retained_versions`）+ 1 个
    /// Golden（success_rate 最高）。回滚候选与 [`SkillPruner`] 清理共用
    /// [`retained_versions`]，Golden 恒纳入两侧口径一致。
    async fn retention_set(&self, name: &str) -> Result<Vec<SkillVersion>, SkillsError> {
        let versions = self.list_versions(name).await?;
        Ok(retained_versions(&versions, self.max_retained_versions))
    }

    /// Agent 可回滚版本列表（仅返回保留范围内的版本号，升序）。
    pub async fn rollback_candidates(&self, name: &str) -> Result<Vec<String>, SkillsError> {
        let mut set = self.retention_set(name).await?;
        set.sort();
        Ok(set.iter().map(SkillVersion::label).collect())
    }

    /// 当前活跃版本号（头指针为唯一权威源：promote 容忍的瞬态双
    /// Active 记录不影响本值）。未版本化的旧数据返回 `None`。
    pub async fn active_version(&self, name: &str) -> Result<Option<String>, SkillsError> {
        Self::validate_name(name)?;
        Ok(self.read_head(name).await?.map(|head| head.active))
    }

    /// 写活跃版本镜像（skills:/skills_meta:）供面板/Loader 读取。
    async fn set_active_mirror(
        &self,
        name: &str,
        content: &str,
        meta: SkillMeta,
    ) -> Result<(), SkillsError> {
        self.put_skill(name, content, meta).await
    }

    /// 内部：将 `content` 写为 `name` 的新活跃版本 `new_ver`，旧活跃版降为
    /// Deprecated，更新头与镜像。返回新版的 SkillSummary。
    ///
    /// 写入顺序按“失败危害最小”排列（KvStore 无多 key 事务）：
    /// 新版本→镜像→头→旧版降级。任意前缀失败都不会留下
    /// “无 Active 版本”或“头指向 Deprecated”的状态；最坏情况是
    /// 短暂存在两个 Active 版本记录（头仍唯一指定生效版）。
    /// 清理由会话结束后的 [`SkillPruner`] 单独触发。
    async fn promote_version(
        &self,
        name: &str,
        mut head: SkillHead,
        new_ver: SkillVersion,
        content: String,
        now: i64,
    ) -> Result<SkillSummary, SkillsError> {
        let prev_active = head.active.clone();
        // 1) 新版本记录先落盘（失败则一切未变）
        let record = VersionRecord {
            checksum: skill_checksum(&content),
            content: content.clone(),
            status: SkillStatus::Active,
            score: SkillScore::default(),
            created_at: now,
        };
        self.write_version(name, &new_ver.label(), &record).await?;
        // 2) 镜像（skills:/skills_meta:，面板/Loader 读取面）。信任级与
        //    创建者沿用头元数据（meta_from_content 固定 Generated/agent，
        //    不得因 update/rollback 改写 system 来源标识）
        let mut meta = Self::meta_from_content(&content, head.meta.created_at);
        meta.trust_level = head.meta.trust_level;
        meta.creator = head.meta.creator.clone();
        self.set_active_mirror(name, &content, meta.clone()).await?;
        // 3) 头指针切换
        head.active = new_ver.label();
        head.meta = meta.clone();
        self.write_head(name, &head).await?;
        // 4) 旧活跃版降级（最后；失败仅留双 Active 记录，头仍正确）
        if prev_active != head.active
            && let Some(mut prev) = self.read_version(name, &prev_active).await?
        {
            prev.status = SkillStatus::Deprecated;
            self.write_version(name, &prev_active, &prev).await?;
        }
        Ok(SkillSummary {
            name: name.to_string(),
            description: meta.description,
            category: meta.category,
            requires_tools: meta.requires_tools,
        })
    }

    /// 解析头指针的活跃版本号；损坏（不可解析）时回退到现存最高
    /// 版本号，保证后续新版本号严格高于全部现存记录（若回退
    /// INITIAL 会覆写既有 v1.1 等版本，静默丢失版本链）；无任何
    /// 版本记录时回退 INITIAL。
    async fn active_or_max_version(
        &self,
        name: &str,
        head_active: &str,
    ) -> Result<SkillVersion, SkillsError> {
        if let Some(ver) = SkillVersion::parse(head_active) {
            return Ok(ver);
        }
        Ok(self
            .list_versions(name)
            .await?
            .last()
            .map(|(ver, _)| *ver)
            .unwrap_or(SkillVersion::INITIAL))
    }

    /// 记录一次技能执行结果；若当前版本连续失败达阈值且保留范围内存在
    /// success_rate 更高的版本，则自动回滚到该版本。返回是否发生自动回滚。
    ///
    /// 并发语义：评分更新为读-改-写（KvStore 无跨 key 事务），并发调用
    /// 可能丢失计数。当前仅由测试调用（运行时接线随 Phase 7+ 技能执行
    /// 回路，见对齐清单）；接线时需按技能名串行化调用（单飞行队列或
    /// per-name 锁），避免丢计数与重复触发自动回滚。
    pub async fn record_outcome(&self, name: &str, ok: bool) -> Result<bool, SkillsError> {
        Self::validate_name(name)?;
        let Some(head) = self.read_head(name).await? else {
            return Err(SkillsError::NotFound(name.to_string()));
        };
        let Some(mut active) = self.read_version(name, &head.active).await? else {
            return Err(SkillsError::NotFound(format!("{name}:{}", head.active)));
        };
        active.score.record(ok);
        self.write_version(name, &head.active, &active).await?;
        if ok || active.score.consecutive_failures < AUTO_ROLLBACK_CONSECUTIVE_FAILURES {
            return Ok(false);
        }
        // 自动回滚：保留范围内成功率严格高于当前的最佳版本
        let active_rate = active.score.success_rate();
        let retention = self.retention_set(name).await?;
        let versions = self.list_versions(name).await?;
        let active_ver = SkillVersion::parse(&head.active);
        let mut best: Option<(SkillVersion, f32)> = None;
        for (ver, rec) in &versions {
            if Some(*ver) == active_ver || !retention.contains(ver) {
                continue;
            }
            let rate = rec.score.success_rate();
            if rate > active_rate && best.map(|(_, r)| rate > r).unwrap_or(true) {
                best = Some((*ver, rate));
            }
        }
        if let Some((target, _)) = best {
            let target_rec = versions.iter().find(|(v, _)| *v == target);
            // 防抖动：候选内容与当前活跃版字节相同时回滚无意义
            //（新版评分归零后再度连续失败会无限重升同一内容），跳过。
            if target_rec
                .map(|(_, r)| r.checksum == active.checksum)
                .unwrap_or(true)
            {
                return Ok(false);
            }
            let content = target_rec
                .map(|(_, r)| r.content.clone())
                .unwrap_or_default();
            let new_ver = active_ver.unwrap_or(SkillVersion::INITIAL).next_major();
            self.promote_version(name, head, new_ver, content, now_ms())
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// 面板列表：全部条目（含损坏标记）。损坏判定：meta 缺失/反序列化失败、
    /// 原文缺失、checksum 不匹配。
    pub async fn list_entries(&self) -> Result<Vec<SkillEntry>, SkillsError> {
        let keys = self
            .kv
            .list_prefix(SKILL_KEY_PREFIX)
            .await
            .map_err(storage_err)?;
        let mut entries = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(name) = key.strip_prefix(SKILL_KEY_PREFIX) else {
                continue;
            };
            entries.push(self.entry_for(name).await?);
        }
        // 只有 meta 的孤儿条目（原文丢失）也要暴露为损坏，供面板删除
        let meta_keys = self
            .kv
            .list_prefix(SKILL_META_KEY_PREFIX)
            .await
            .map_err(storage_err)?;
        for key in meta_keys {
            let Some(name) = key.strip_prefix(SKILL_META_KEY_PREFIX) else {
                continue;
            };
            if entries.iter().any(|e| e.name == name) {
                continue;
            }
            entries.push(SkillEntry {
                name: name.to_string(),
                meta: None,
                corrupted: true,
            });
        }
        // 头孤儿（create 部分写入中断：有 head/版本但无镜像）同样暴露，
        // 避免残留条目对面板不可见且无法删除
        let head_keys = self
            .kv
            .list_prefix(SKILL_HEAD_KEY_PREFIX)
            .await
            .map_err(storage_err)?;
        for key in head_keys {
            let Some(name) = key.strip_prefix(SKILL_HEAD_KEY_PREFIX) else {
                continue;
            };
            if entries.iter().any(|e| e.name == name) {
                continue;
            }
            entries.push(SkillEntry {
                name: name.to_string(),
                meta: None,
                corrupted: true,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn entry_for(&self, name: &str) -> Result<SkillEntry, SkillsError> {
        // 存储错误上报（? 传播），仅内容缺失/非字符串才视为损坏，
        // 避免瞬时读错误把健康 skill 误标为可删除的损坏条目。
        let raw = match self.raw_content(name).await {
            Ok(raw) => raw,
            // 非字符串载荷（内容本身损坏）归为损坏；其余存储错误上报
            Err(SkillsError::Storage(msg)) if msg.contains("non-string payload") => None,
            Err(err) => return Err(err),
        };
        let meta = match self
            .kv
            .get(&Self::meta_key(name))
            .await
            .map_err(storage_err)?
        {
            Some(value) => serde_json::from_value::<SkillMeta>(value).ok(),
            None => None,
        };
        let corrupted = match (&raw, &meta) {
            (Some(content), Some(meta)) => skill_checksum(content) != meta.checksum,
            _ => true,
        };
        Ok(SkillEntry {
            name: name.to_string(),
            meta,
            corrupted,
        })
    }
}

fn storage_err(err: MemoryError) -> SkillsError {
    SkillsError::Storage(err.to_string())
}

fn gate_matches(meta: &SkillMeta, ctx: &SkillContext) -> bool {
    let platform_ok = meta.platforms.is_empty() || meta.platforms.contains(&ctx.platform);
    let tools_ok = meta
        .requires_tools
        .iter()
        .all(|tool| ctx.available_tools.iter().any(|t| t == tool));
    platform_ok && tools_ok
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl SkillLoader for KvSkillStore {
    /// Level 0：门控过滤后的摘要列表；损坏条目不进入模型可见面
    /// （面板经 [`KvSkillStore::list_entries`] 仍可见并删除）。
    async fn list(&self, ctx: &SkillContext) -> Result<Vec<SkillSummary>, SkillsError> {
        let entries = self.list_entries().await?;
        Ok(entries
            .into_iter()
            .filter(|entry| !entry.corrupted)
            .filter_map(|entry| {
                let meta = entry.meta?;
                gate_matches(&meta, ctx).then_some(SkillSummary {
                    name: entry.name,
                    description: meta.description,
                    category: meta.category,
                    requires_tools: meta.requires_tools,
                })
            })
            .collect())
    }

    /// Level 1：完整 SKILL.md（frontmatter 解析失败按 InvalidFormat 上报）。
    /// 完整性门控：无 meta 或 checksum 不匹配的条目不可注入模型上下文
    /// （与 list 同口径，防止经名称旁路加载已篡改内容）。
    async fn load(&self, name: &str) -> Result<SkillContent, SkillsError> {
        Self::validate_name(name)?;
        let raw = self
            .raw_content(name)
            .await?
            .ok_or_else(|| SkillsError::NotFound(name.to_string()))?;
        let meta = match self
            .kv
            .get(&Self::meta_key(name))
            .await
            .map_err(storage_err)?
        {
            Some(value) => serde_json::from_value::<SkillMeta>(value).ok(),
            None => None,
        };
        match meta {
            Some(meta) if skill_checksum(&raw) == meta.checksum => {}
            _ => {
                return Err(SkillsError::InvalidFormat(format!(
                    "skill `{name}` failed integrity verification"
                )));
            }
        }
        let (frontmatter_raw, body) = split_frontmatter(&raw);
        let frontmatter = if frontmatter_raw.is_empty() {
            serde_yaml::Value::Null
        } else {
            serde_yaml::from_str(&frontmatter_raw)
                .map_err(|e| SkillsError::InvalidFormat(format!("frontmatter: {e}")))?
        };
        Ok(SkillContent { frontmatter, body })
    }

    /// Level 2：加载技能引用文件（references/ 、templates/ 等）。
    /// 存于 `skills_ref:{name}:{path}`；Agent 可经 `put_reference` 写入。
    /// 完整性门控与 `load` 同口径：checksum 缺失/不匹配的引用不可
    /// 注入模型上下文（防止经引用路径旁路注入已篡改内容）。
    async fn load_reference(&self, name: &str, path: &str) -> Result<String, SkillsError> {
        Self::validate_name(name)?;
        let Some(value) = self
            .kv
            .get(&Self::ref_key(name, path))
            .await
            .map_err(storage_err)?
        else {
            return Err(SkillsError::NotFound(format!("{name}/{path}")));
        };
        let verified = value.as_object().and_then(|obj| {
            let content = obj.get("content")?.as_str()?;
            let checksum = obj.get("checksum")?.as_str()?;
            (skill_checksum(content) == checksum).then(|| content.to_string())
        });
        verified.ok_or_else(|| {
            SkillsError::InvalidFormat(format!(
                "skill reference `{name}/{path}` failed integrity verification"
            ))
        })
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl SkillManage for KvSkillStore {
    /// Agent 自主创建：初始版本 v1.0（Active）。已存在则报错（改用 update）。
    async fn create_skill(&self, name: &str, content: &str) -> Result<SkillSummary, SkillsError> {
        Self::validate_name(name)?;
        if content.len() > MAX_SKILL_CONTENT_BYTES {
            return Err(SkillsError::InvalidFormat(format!(
                "skill content exceeds {MAX_SKILL_CONTENT_BYTES} bytes"
            )));
        }
        if self.read_head(name).await?.is_some() {
            return Err(SkillsError::InvalidFormat(format!(
                "skill `{name}` already exists; use update_skill"
            )));
        }
        let now = now_ms();
        let meta = Self::meta_from_content(content, now);
        let ver = SkillVersion::INITIAL;
        let record = VersionRecord {
            checksum: skill_checksum(content),
            content: content.to_string(),
            status: SkillStatus::Active,
            score: SkillScore::default(),
            created_at: now,
        };
        self.write_version(name, &ver.label(), &record).await?;
        // 镜像先于头：中途失败时条目对面板可见可删；头孤儿另由
        // list_entries 兜底暴露
        self.set_active_mirror(name, content, meta.clone()).await?;
        self.write_head(
            name,
            &SkillHead {
                active: ver.label(),
                meta: meta.clone(),
            },
        )
        .await?;
        Ok(SkillSummary {
            name: name.to_string(),
            description: meta.description,
            category: meta.category,
            requires_tools: meta.requires_tools,
        })
    }

    /// 更新：小版本递增（流程微调），新版 Active、旧活跃版降为 Deprecated。
    async fn update_skill(&self, name: &str, content: &str) -> Result<SkillSummary, SkillsError> {
        Self::validate_name(name)?;
        if content.len() > MAX_SKILL_CONTENT_BYTES {
            return Err(SkillsError::InvalidFormat(format!(
                "skill content exceeds {MAX_SKILL_CONTENT_BYTES} bytes"
            )));
        }
        let Some(head) = self.read_head(name).await? else {
            return Err(SkillsError::NotFound(name.to_string()));
        };
        let active_ver = self.active_or_max_version(name, &head.active).await?;
        let new_ver = active_ver.next_minor();
        self.promote_version(name, head, new_ver, content.to_string(), now_ms())
            .await
    }

    /// 回滚：目标版本内容提升为新大版本（版本链只增不删）。
    /// 目标必须在保留范围内（最近 3 + Golden）。
    async fn rollback_skill(
        &self,
        name: &str,
        target_version: &str,
    ) -> Result<SkillSummary, SkillsError> {
        Self::validate_name(name)?;
        let Some(head) = self.read_head(name).await? else {
            return Err(SkillsError::NotFound(name.to_string()));
        };
        let Some(target) = SkillVersion::parse(target_version) else {
            return Err(SkillsError::InvalidFormat(format!(
                "invalid version: {target_version}"
            )));
        };
        if !self.retention_set(name).await?.contains(&target) {
            return Err(SkillsError::InvalidFormat(format!(
                "rollback target {target_version} is outside the retained range"
            )));
        }
        let content = self
            .read_version(name, target_version)
            .await?
            .ok_or_else(|| SkillsError::NotFound(format!("{name}:{target_version}")))?
            .content;
        let active_ver = self.active_or_max_version(name, &head.active).await?;
        let new_ver = active_ver.next_major();
        self.promote_version(name, head, new_ver, content, now_ms())
            .await
    }

    /// 删除：清除全部版本/引用/头 + 活跃版镜像；均不存在报 NotFound。
    ///
    /// 前缀删除先于 NotFound 判定无条件执行（幂等）：create 中断可能只
    /// 残留 `skills_ver:` 记录，否则既不可见也无法回收（存储泄漏）。
    async fn delete_skill(&self, name: &str) -> Result<(), SkillsError> {
        Self::validate_name(name)?;
        let content_key = Self::content_key(name);
        let meta_key = Self::meta_key(name);
        let head_key = Self::head_key(name);
        let had_any = self
            .kv
            .get(&content_key)
            .await
            .map_err(storage_err)?
            .is_some()
            || self.kv.get(&meta_key).await.map_err(storage_err)?.is_some()
            || self.kv.get(&head_key).await.map_err(storage_err)?.is_some();
        let removed_orphans = self
            .kv
            .delete_prefix(&Self::ver_prefix(name))
            .await
            .map_err(storage_err)?
            + self
                .kv
                .delete_prefix(&Self::ref_prefix(name))
                .await
                .map_err(storage_err)?;
        if !had_any && removed_orphans == 0 {
            return Err(SkillsError::NotFound(name.to_string()));
        }
        self.kv.delete(&content_key).await.map_err(storage_err)?;
        self.kv.delete(&meta_key).await.map_err(storage_err)?;
        self.kv.delete(&head_key).await.map_err(storage_err)?;
        Ok(())
    }
}

impl KvSkillStore {
    /// 写入 Level 2 引用文件（references/ 、templates/ 等）。
    /// 与 SKILL.md 同样携带 checksum（content + sha256 同 key 落盘），
    /// `load_reference` 读时验证。
    pub async fn put_reference(
        &self,
        name: &str,
        path: &str,
        content: &str,
    ) -> Result<(), SkillsError> {
        Self::validate_name(name)?;
        if path.trim().is_empty() || path.contains(':') {
            return Err(SkillsError::InvalidFormat(format!(
                "invalid reference path: {path}"
            )));
        }
        if content.len() > MAX_SKILL_CONTENT_BYTES {
            return Err(SkillsError::InvalidFormat("reference too large".into()));
        }
        let payload = serde_json::json!({
            "content": content,
            "checksum": skill_checksum(content),
        });
        self.kv
            .set(&Self::ref_key(name, path), &payload, None)
            .await
            .map_err(storage_err)
    }
}

/// 从版本列表计算保留集（回滚候选与清理共用的单一权威逻辑）：
/// 最近 `max_retained` 个版本 + 1 个 Golden（success_rate 最高，并列
/// 取版本号更高者）。活跃版恒为最高版本号 → 必在最近 N 内，
/// 无需单独兵底。
fn retained_versions(
    versions: &[(SkillVersion, VersionRecord)],
    max_retained: usize,
) -> Vec<SkillVersion> {
    let mut retained: Vec<SkillVersion> = versions
        .iter()
        .rev()
        .take(max_retained)
        .map(|(v, _)| *v)
        .collect();
    if let Some((golden, _)) = versions.iter().max_by(|a, b| {
        a.1.score
            .success_rate()
            .partial_cmp(&b.1.score.success_rate())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    }) && !retained.contains(golden)
    {
        retained.push(*golden);
    }
    retained
}

/// Skill 清理器（会话结束后触发）：保留最近 N 版本 + Golden，其余按
/// success_rate 升序淘汰（第六章清理策略）。保留集与回滚候选完全
/// 共用 [`retained_versions`]（窗口 N 以目标 [`KvSkillStore`] 配置为准），
/// 因此不会出现“清理保留的版本回滚却拒用”。
#[derive(Default)]
pub struct SkillPruner;

impl SkillPruner {
    /// 对指定 Skill 执行清理：保留最近 N 版本 + Golden，其余按评分升序
    /// 逐个删除（活跃版恒在保留集内，不会被删）。返回本次删除的版本数。
    pub async fn prune(&self, store: &KvSkillStore, name: &str) -> Result<usize, SkillsError> {
        let versions = store.list_versions(name).await?;
        if versions.len() <= store.max_retained_versions {
            return Ok(0);
        }
        // 与回滚候选同口径的保留集（最近 N + Golden）
        let retained = retained_versions(&versions, store.max_retained_versions);
        // 淘汰集按 success_rate 升序
        let mut evict: Vec<&(SkillVersion, VersionRecord)> = versions
            .iter()
            .filter(|(v, _)| !retained.contains(v))
            .collect();
        evict.sort_by(|a, b| {
            a.1.score
                .success_rate()
                .partial_cmp(&b.1.score.success_rate())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut removed = 0;
        for (ver, _) in evict {
            store
                .kv
                .delete(&KvSkillStore::version_key(name, &ver.label()))
                .await
                .map_err(storage_err)?;
            removed += 1;
        }
        Ok(removed)
    }
}
