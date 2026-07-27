//! Tool Runtime 统一抽象（对齐 OpenHarness `tools/base.py`）。
//!
//! 所有工具（本地 / 远程 MCP / AgentInternal）实现同一 `Tool` trait，经统一
//! 注册表 + 三态权限 + hooks 分发，Kernel 不感知工具来源；注册表、权限引擎与
//! hooks 在 Phase 3 落地。

use std::path::Path;

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

/// 跨轮工具状态袋占位；Phase 1.5 收敛为带条数上限的结构化状态袋。
pub type ToolMetadata = serde_json::Map<String, Value>;

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

    /// 按参数自报只读性：同一工具可因参数不同而权限不同，
    /// 由 PermissionChecker 结合 PermissionMode 决策（基线默认 `false`）。
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError>;

    fn category(&self) -> ToolCategory;
}
