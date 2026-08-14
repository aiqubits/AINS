//! Skills System 抽象（AINS_PLAN 第六章，格式对齐 agentskills.io / Harness `skills/`）。
//!
//! Skills 是 Tools 的“说明书”，不提供新能力；存储随 KvStore 后端走，
//! 支持标准目录包导入/导出与显式命令创建。渐进式加载三层：
//! Level 0 摘要列表 → Level 1 全文 → Level 2 引用文件。实现在 Phase 6。

use serde::{Deserialize, Serialize};

use crate::error::SkillsError;
use crate::marker::MaybeSendSync;
use crate::platform::Platform;

pub mod files;
pub mod store;

pub use files::{SkillFiles, open_platform_skill_files, schema_stub_skill_files};
pub use store::{
    AUTO_ROLLBACK_CONSECUTIVE_FAILURES, DEFAULT_MAX_RETAINED_VERSIONS, MAX_SKILL_MD_BYTES,
    MAX_SKILL_NAME_CHARS, MAX_SKILL_PACKAGE_BYTES, MAX_SKILL_PACKAGE_FILES,
    MAX_SKILL_RESOURCE_BYTES, SKILL_INDEX_KEY_PREFIX, SKILL_MD_FILE, SKILL_META_KEY_PREFIX,
    SKILL_RUNTIME_KEY_PREFIX, SkillEntry, SkillHead, SkillIndex, SkillMeta, SkillPackage,
    SkillPruner, SkillScore, SkillStatus, SkillStore, SkillTrust, SkillVersion, VersionRecord,
    skill_checksum, split_frontmatter, validate_agent_skill_package,
};

/// Skill 门控上下文：`list` 阶段即过滤，不匹配的 skill 完全不可见。
#[derive(Debug, Clone)]
pub struct SkillContext {
    pub platform: Platform,
    pub available_tools: Vec<String>,
}

/// Level 0 摘要（启动时注入 System Prompt）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub category: String,
    pub requires_tools: Vec<String>,
}

/// Level 1 全文：YAML frontmatter + Markdown body。
#[derive(Debug, Clone)]
pub struct SkillContent {
    pub frontmatter: serde_yaml::Value,
    pub body: String,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait SkillLoader: MaybeSendSync {
    /// Level 0：返回当前运行时上下文中可用的 skill 摘要列表（经门控过滤）。
    async fn list(&self, ctx: &SkillContext) -> Result<Vec<SkillSummary>, SkillsError>;

    /// Level 1：按 name 加载完整 skill 内容。
    async fn load(&self, name: &str) -> Result<SkillContent, SkillsError>;

    /// Level 2：加载 skill 引用文件（references/、templates/ 等）。
    async fn load_reference(&self, name: &str, path: &str) -> Result<String, SkillsError>;
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait SkillManage: MaybeSendSync {
    /// 用户通过智能对话页的显式创建命令保存工作流。
    async fn create_skill(&self, name: &str, content: &str) -> Result<SkillSummary, SkillsError>;

    async fn update_skill(&self, name: &str, content: &str) -> Result<SkillSummary, SkillsError>;

    /// 回滚：目标版本内容提升为新版本，版本链只增不删（见第六章回滚机制）。
    async fn rollback_skill(
        &self,
        name: &str,
        target_version: &str,
    ) -> Result<SkillSummary, SkillsError>;

    async fn delete_skill(&self, name: &str) -> Result<(), SkillsError>;

    /// 删除当前存储域内的全部技能（包括孤立版本和引用文件）。返回移除的键数。
    async fn clear_all_skills(&self) -> Result<u64, SkillsError>;
}
