//! Tool Runtime 统一抽象（对齐 Harness `tools/base.py`）。
//!
//! 所有工具（本地 / 远程 MCP / AgentInternal）实现同一 `Tool` trait，经统一
//! 注册表（`runtime::ToolRuntime`）+ 三态权限（`policy`）+ hooks（`hooks`）
//! 分发，Kernel 不感知工具来源。

pub mod compute;
pub mod interact;
pub mod mcp;
pub mod memory;
pub mod network;
pub mod outputs;
pub mod runtime;
pub mod skills;

#[cfg(not(target_arch = "wasm32"))]
pub mod filesystem;
#[cfg(not(target_arch = "wasm32"))]
pub mod system;

pub use memory::{MemoryReadTool, MemoryWriteTool, MemoryWriter};
pub use runtime::ToolRuntime;
pub use skills::{SkillCreateTool, SkillLoadTool, SkillResourceLoadTool};

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolError;
use crate::marker::MaybeSendSync;

/// 工具定义：`input_schema` 为 JSON Schema，随模型请求下发
/// （对齐基线 `to_api_schema()` 的 name / description / input_schema 三元组）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// 归一化执行结果（对齐基线 `ToolResult`：output / is_error / metadata）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
    pub metadata: Value,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
            metadata: Value::Null,
        }
    }

    pub fn err(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: true,
            metadata: Value::Null,
        }
    }
}

/// 单个状态袋列表键的条数上限（超限淘汰最旧条目）。
pub const TOOL_METADATA_LIST_CAP: usize = 50;

/// 跨轮工具状态袋（对齐基线 `_record_tool_carryover` 的 capped-unique 语义）：
/// 已读文件 / 已调技能 / 用户目标 / 工作日志四个受限键 + 工具自定义 `extra`。
/// 随会话快照按白名单持久化（快照机制于后续 Phase 落地）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolMetadata {
    pub read_files: Vec<String>,
    pub invoked_skills: Vec<String>,
    pub user_goal: Option<String>,
    pub work_log: Vec<String>,
    /// 超长工具输出外置后的活跃工件引用（对齐基线
    /// `_remember_active_artifact` 的独立记录，不占 work_log 配额）。
    #[serde(default)]
    pub active_artifacts: Vec<String>,
    /// 工具自定义键值（无固定 schema，不参与条数上限治理）。
    #[serde(default)]
    pub extra: serde_json::Map<String, Value>,
}

impl ToolMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录已读文件：去重后移到末尾（最近使用在后），超限淘汰最旧。
    pub fn record_read_file(&mut self, path: impl Into<String>) {
        Self::push_capped_unique(&mut self.read_files, path.into());
    }

    /// 记录已调技能：capped-unique，语义同 `record_read_file`。
    pub fn record_invoked_skill(&mut self, name: impl Into<String>) {
        Self::push_capped_unique(&mut self.invoked_skills, name.into());
    }

    pub fn set_user_goal(&mut self, goal: impl Into<String>) {
        self.user_goal = Some(goal.into());
    }

    /// 追加工作日志条目：capped-unique，保留最近条目。
    pub fn append_work_log(&mut self, entry: impl Into<String>) {
        Self::push_capped_unique(&mut self.work_log, entry.into());
    }

    /// 记录活跃工件引用：capped-unique，独立于 work_log（review 二轮修复：
    /// 工件引用不得挤占真实工作日志的 50 条配额）。
    pub fn record_active_artifact(&mut self, reference: impl Into<String>) {
        Self::push_capped_unique(&mut self.active_artifacts, reference.into());
    }

    fn push_capped_unique(list: &mut Vec<String>, value: String) {
        if let Some(position) = list.iter().position(|existing| *existing == value) {
            list.remove(position);
        }
        list.push(value);
        if list.len() > TOOL_METADATA_LIST_CAP {
            let overflow = list.len() - TOOL_METADATA_LIST_CAP;
            list.drain(0..overflow);
        }
    }
}

/// 工具执行上下文（对齐基线 `ToolExecutionContext` 的 cwd + metadata；
/// hooks 通道随 Phase 3 Hook System 加入）。
pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub metadata: &'a mut ToolMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    Compute,
    FileSystem,
    Browser,
    System,
    Network,
    /// Agent 内部工具（memory_* / skill_* / context_* 等），委托 RuntimeServices。
    AgentInternal,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait Tool: MaybeSendSync {
    fn definition(&self) -> ToolDef;

    /// Whether this registered tool may be advertised to and called by the
    /// model for the current turn.  Most tools are always available; narrowly
    /// authorized host actions can opt in only while their user-granted token
    /// is live.
    fn is_available(&self) -> bool {
        true
    }

    /// 按参数自报只读性：同一工具可因参数不同而权限不同，
    /// 由 PermissionChecker 结合 PermissionMode 决策（基线默认 `false`）。
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    /// 返回必须按调用顺序独占执行的逻辑资源键。一个 assistant turn 中若
    /// 多个工具声明同一键，Runtime 会回退为共享 metadata 的顺序分发，避免
    /// 基于同一快照的 read-modify-write 丢失更新。路径类资源键必须用
    /// `cwd` 锚定后的规范路径，否则相对/绝对路径别名会绕过冲突检测。
    fn exclusive_execution_key(&self, _input: &Value, _cwd: &Path) -> Option<String> {
        None
    }

    /// 注入本轮查询的协作式取消标志（Phase 7.1 review 接线）：Kernel 在工具
    /// 批分发前调用。长时工具（如 shell）应把它透传给沙箱后端的
    /// [`crate::policy::ShellRequest::cancel`]，使 UI 中断能终止运行中的进程树；
    /// 其余工具默认不消费（无长时操作，忽略即可）。每次分发前 Runtime 都会
    /// 重新注入（含 None 清除），无需工具自行清理。
    fn set_query_cancel(&self, _flag: Option<Arc<AtomicBool>>) {}

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError>;

    fn category(&self) -> ToolCategory;
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn tool_metadata_lists_are_capped_unique() {
        let mut metadata = ToolMetadata::new();
        for i in 0..(TOOL_METADATA_LIST_CAP + 10) {
            metadata.record_read_file(format!("file_{i}"));
        }
        assert_eq!(metadata.read_files.len(), TOOL_METADATA_LIST_CAP);
        // 最旧的 10 条被淘汰，最新条目在末尾
        assert_eq!(metadata.read_files.first().unwrap(), "file_10");
        assert_eq!(
            metadata.read_files.last().unwrap(),
            &format!("file_{}", TOOL_METADATA_LIST_CAP + 9)
        );

        // 重复记录移动到末尾而非重复插入
        metadata.record_read_file("file_20");
        assert_eq!(metadata.read_files.len(), TOOL_METADATA_LIST_CAP);
        assert_eq!(metadata.read_files.last().unwrap(), "file_20");
    }

    #[test]
    fn tool_metadata_goal_and_work_log() {
        let mut metadata = ToolMetadata::new();
        metadata.set_user_goal("整理报表");
        metadata.append_work_log("解析 CSV");
        metadata.append_work_log("解析 CSV");
        metadata.record_invoked_skill("csv-report-workflow");
        assert_eq!(metadata.user_goal.as_deref(), Some("整理报表"));
        assert_eq!(metadata.work_log, vec!["解析 CSV"]);
        assert_eq!(metadata.invoked_skills, vec!["csv-report-workflow"]);
    }

    #[test]
    fn tool_metadata_serde_roundtrip_with_extra() {
        let mut metadata = ToolMetadata::new();
        metadata.record_read_file("a.txt");
        metadata
            .extra
            .insert("echo_calls".into(), serde_json::json!(1));
        let json = serde_json::to_value(&metadata).unwrap();
        let back: ToolMetadata = serde_json::from_value(json).unwrap();
        assert_eq!(back, metadata);
    }
}
