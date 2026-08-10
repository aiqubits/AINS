//! 交互 / 元工具（对齐 OpenHarness `todo_write_tool.py` / `ask_user_question_tool.py`
//! / `enter_plan_mode_tool.py` / `exit_plan_mode_tool.py`）。
//!
//! - `todo_write`：markdown 清单增改（Native 落 cwd 下文件；Web 无文件系统，
//!   清单存 tool_metadata 状态袋 `extra["todo_markdown:{path}"]`——偏差记录）。
//! - `ask_user_question`：异步回调回 UI（基线经 context.metadata 传可调用对象，
//!   AINS 以构造注入 `UserInteraction` trait 对象——语义等价）。
//! - `enter/exit_plan_mode`：切换共享权限引擎的 PermissionMode（基线写配置
//!   文件；AINS 模式持久化随会话快照落地，Phase 5）。

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::error::ToolError;
use crate::marker::MaybeSendSync;
use crate::policy::{PermissionEngine, PermissionMode};
use crate::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

const TODO_DOCUMENT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// TODO 文档的词法路径归一化。Web 端以路径作为状态键，必须让
/// `TODO.md` / `./TODO.md` / `notes/../TODO.md` 指向同一份虚拟文档；
/// 同一归一化值也用于 Web 端的并发独占键（Native 端独占键改用
/// `filesystem::file_resource_key` 与写文件工具同口径）。
#[cfg(any(target_arch = "wasm32", test))]
fn normalize_todo_path(path: &str) -> String {
    use std::path::{Component, PathBuf};

    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    normalized.components().next_back(),
                    None | Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        ".".to_string()
    } else {
        normalized.to_string_lossy().into_owned()
    }
}

// ── todo_write ──────────────────────────────────────────────────────────

/// 对 markdown 清单文本应用一次 TODO 增改（对齐 TodoWriteTool.execute 的
/// 纯文本变换部分）。返回 `None` 表示无需变更（幂等命中）。
///
/// 偏差（有意，review 二轮修复）：基线用整串子串 `in`/`replace`，
/// 短 item 会误命中长条目前缀（item "a" 误勾 "- [ ] ab"）；AINS 按
/// **行边界**匹配，对齐清单语义而非基线缺陷。
pub fn apply_todo(existing: &str, item: &str, checked: bool) -> Option<String> {
    let unchecked_line = format!("- [ ] {item}");
    let checked_line = format!("- [x] {item}");
    let target_line = if checked {
        &checked_line
    } else {
        &unchecked_line
    };

    let has_line = |needle: &str| existing.lines().any(|line| line.trim_end() == needle);

    if checked && has_line(&unchecked_line) {
        // 未勾选项就地标记完成（仅首个命中行）
        let mut replaced = false;
        let mut lines: Vec<String> = Vec::new();
        for line in existing.lines() {
            if !replaced && line.trim_end() == unchecked_line {
                lines.push(checked_line.clone());
                replaced = true;
            } else {
                lines.push(line.to_string());
            }
        }
        let mut updated = lines.join("\n");
        if existing.ends_with('\n') {
            updated.push('\n');
        }
        Some(updated)
    } else if has_line(target_line) {
        // 已处于目标状态：no-op
        None
    } else {
        // 新条目追加
        Some(format!("{}\n{target_line}\n", existing.trim_end()))
    }
}

/// 在 markdown 清单文件中新增 TODO 或标记完成。
pub struct TodoWriteTool;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for TodoWriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "todo_write".into(),
            description: "Add a new TODO item or mark an existing one as done in a markdown \
                          checklist file."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "item": {
                        "type": "string",
                        "minLength": 1,
                        "pattern": "^[^\\r\\n]*\\S[^\\r\\n]*$",
                        "description": "Single-line, non-empty TODO item text"
                    },
                    "checked": {"type": "boolean", "default": false},
                    "path": {"type": "string", "default": "TODO.md"}
                },
                "required": ["item"]
            }),
        }
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let item = input
            .get("item")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("missing required string field: item".into()))?;
        if item.contains(['\r', '\n']) {
            return Ok(ToolResult::err("TODO item must be a single line"));
        }
        let item = item.trim();
        if item.is_empty() {
            return Ok(ToolResult::err("TODO item must not be empty"));
        }
        if item.len() > TODO_DOCUMENT_MAX_BYTES {
            return Ok(ToolResult::err(format!(
                "TODO item is larger than the {TODO_DOCUMENT_MAX_BYTES} byte limit"
            )));
        }
        let checked = input
            .get("checked")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let rel_path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("TODO.md");

        #[cfg(not(target_arch = "wasm32"))]
        {
            // Native 端与 filesystem 工具共用同一路径解析（review 十二轮
            // 修复）：`~` 展开 + cwd 锚定 + 词法归一，保证权限管线求值的
            // 路径与实际写入路径一致（不把跨 cwd 路径钳回 cwd）。
            use std::io::{Seek, SeekFrom, Write};

            // Use the same descriptor-relative, no-symlink traversal as the
            // file tools.  Keep this descriptor for the final write so an
            // attacker cannot swap the TODO path between read and write.
            let (mut file, path) = match crate::tools::filesystem::open_workspace_file_for_update(
                ctx.cwd, rel_path, true,
            ) {
                Ok(opened) => opened,
                Err(reason) => return Ok(ToolResult::err(reason.to_string())),
            };
            let Some(raw) = crate::tools::filesystem::read_open_file_bounded(
                file.try_clone()
                    .map_err(|error| ToolError::Execution(error.to_string()))?,
            )
            .map_err(|error| ToolError::Execution(error.to_string()))?
            else {
                return Ok(ToolResult::err(format!(
                    "TODO file is larger than the {} byte edit limit: {}",
                    crate::tools::filesystem::FILE_INPUT_MAX_BYTES,
                    path.display()
                )));
            };
            let existing = if raw.is_empty() {
                "# TODO\n".to_string()
            } else {
                String::from_utf8(raw).map_err(|error| ToolError::Execution(error.to_string()))?
            };
            match apply_todo(&existing, item, checked) {
                None => Ok(ToolResult::ok(format!(
                    "No change needed in {}",
                    path.display()
                ))),
                Some(updated) => {
                    if updated.len() > TODO_DOCUMENT_MAX_BYTES {
                        return Ok(ToolResult::err(format!(
                            "TODO document would exceed the {TODO_DOCUMENT_MAX_BYTES} byte limit"
                        )));
                    }
                    file.set_len(0)
                        .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
                        .and_then(|()| file.write_all(updated.as_bytes()))
                        .map_err(|error| ToolError::Execution(error.to_string()))?;
                    Ok(ToolResult::ok(format!("Updated {}", path.display())))
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        {
            // Web 无文件系统：清单落 tool_metadata 状态袋（随会话快照持久化）
            let normalized_path = normalize_todo_path(rel_path);
            let key = format!("todo_markdown:{normalized_path}");
            let existing = ctx
                .metadata
                .extra
                .get(&key)
                .and_then(Value::as_str)
                .unwrap_or("# TODO\n");
            if existing.len() > TODO_DOCUMENT_MAX_BYTES {
                return Ok(ToolResult::err(format!(
                    "TODO document is larger than the {TODO_DOCUMENT_MAX_BYTES} byte limit"
                )));
            }
            match apply_todo(existing, item, checked) {
                None => Ok(ToolResult::ok(format!(
                    "No change needed in {normalized_path}"
                ))),
                Some(updated) => {
                    if updated.len() > TODO_DOCUMENT_MAX_BYTES {
                        return Ok(ToolResult::err(format!(
                            "TODO document would exceed the {TODO_DOCUMENT_MAX_BYTES} byte limit"
                        )));
                    }
                    ctx.metadata.extra.insert(key, Value::String(updated));
                    Ok(ToolResult::ok(format!("Updated {normalized_path}")))
                }
            }
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }

    fn exclusive_execution_key(&self, input: &Value, cwd: &Path) -> Option<String> {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("TODO.md");
        // Native 落盘真实文件：与 write_file/edit_file 共用 `file:` 命名
        // 空间，同批次内跨工具命中同一文件时也能回退顺序执行；
        // 并用 resolve_path（~ 展开 + cwd 锚定）封堵相对/绝对别名。
        #[cfg(not(target_arch = "wasm32"))]
        {
            Some(crate::tools::filesystem::file_resource_key(cwd, path))
        }
        // Web 无文件系统：状态袋键自成命名空间，词法归一即可。
        #[cfg(target_arch = "wasm32")]
        {
            let _ = cwd;
            Some(format!("todo_write:{}", normalize_todo_path(path)))
        }
    }
}

// ── ask_user_question ───────────────────────────────────────────────────

/// UI 交互回调：由宿主（Dioxus 前端）实现并注入。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait UserInteraction: MaybeSendSync {
    /// 向交互用户提问并等待答复。
    async fn ask(&self, question: &str) -> String;
}

/// 向交互用户提出后续问题并返回答复（只读）。
pub struct AskUserQuestionTool {
    interaction: Option<Arc<dyn UserInteraction>>,
}

impl AskUserQuestionTool {
    pub fn new(interaction: Option<Arc<dyn UserInteraction>>) -> Self {
        Self { interaction }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for AskUserQuestionTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "ask_user_question".into(),
            description: "Ask the interactive user a follow-up question and return the answer."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "The exact question to ask the user"}
                },
                "required": ["question"]
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
        let question = input
            .get("question")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidInput("missing required string field: question".into())
            })?;
        let Some(interaction) = &self.interaction else {
            return Ok(ToolResult::err(
                "ask_user_question is unavailable in this session",
            ));
        };
        let answer = interaction.ask(question).await.trim().to_string();
        if answer.is_empty() {
            Ok(ToolResult::ok("(no response)"))
        } else {
            Ok(ToolResult::ok(answer))
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }
}

// ── enter / exit plan mode ──────────────────────────────────────────────

/// 切换权限模式为 plan。
pub struct EnterPlanModeTool {
    engine: Arc<PermissionEngine>,
}

impl EnterPlanModeTool {
    pub fn new(engine: Arc<PermissionEngine>) -> Self {
        Self { engine }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for EnterPlanModeTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "enter_plan_mode".into(),
            description: "Switch permission mode to plan.".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    /// 收紧权限（default/full_auto → plan）是安全方向的切换，标记只读使其在
    /// default 模式下免确认（偏差记录：基线非只读，依赖 UI 侧放行）。
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        _input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.engine.set_mode(PermissionMode::Plan);
        ctx.metadata.append_work_log("Entered plan mode");
        Ok(ToolResult::ok("Permission mode set to plan"))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }
}

/// 切换权限模式回 default。
pub struct ExitPlanModeTool {
    engine: Arc<PermissionEngine>,
}

impl ExitPlanModeTool {
    pub fn new(engine: Arc<PermissionEngine>) -> Self {
        Self { engine }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for ExitPlanModeTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "exit_plan_mode".into(),
            description: "Switch permission mode back to default.".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    // 放宽权限（plan → default）非只读：plan 模式下经权限引擎的
    // 显式 allow（宿主应将 exit_plan_mode 列入 allowed_tools 或经确认回调），
    // 防止模型静默逃出 plan 模式（对齐基线安全语义）。

    async fn execute(
        &self,
        _input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.engine.set_mode(PermissionMode::Default);
        ctx.metadata.append_work_log("Exited plan mode");
        Ok(ToolResult::ok("Permission mode set to default"))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::policy::PermissionSettings;
    use crate::tools::ToolMetadata;
    use std::path::Path;

    #[test]
    fn apply_todo_matrix() {
        // 新条目追加
        let updated = apply_todo("# TODO\n", "write tests", false).unwrap();
        assert_eq!(updated, "# TODO\n- [ ] write tests\n");
        // 标记完成（就地）
        let updated = apply_todo(&updated, "write tests", true).unwrap();
        assert_eq!(updated, "# TODO\n- [x] write tests\n");
        // 已完成再标记：no-op
        assert!(apply_todo(&updated, "write tests", true).is_none());
        // 已存在未勾选项再次添加：no-op
        let text = "# TODO\n- [ ] a\n";
        assert!(apply_todo(text, "a", false).is_none());
        // 直接以 checked 添加新条目
        let updated = apply_todo("# TODO\n", "done thing", true).unwrap();
        assert_eq!(updated, "# TODO\n- [x] done thing\n");
    }

    #[test]
    fn apply_todo_matches_line_boundaries_not_prefixes() {
        // review 二轮修复回归：短 item 不得误命中长条目前缀
        let text = "# TODO\n- [ ] ab\n";
        // item "a" 不存在：新增而非误判 no-op
        let updated = apply_todo(text, "a", false).unwrap();
        assert_eq!(updated, "# TODO\n- [ ] ab\n- [ ] a\n");
        // 对 "a" 打勾：不得把 "- [ ] ab" 勾成 "- [x] ab"
        let updated = apply_todo(&updated, "a", true).unwrap();
        assert_eq!(updated, "# TODO\n- [ ] ab\n- [x] a\n");
    }

    #[test]
    fn todo_path_aliases_share_one_resource_key() {
        assert_eq!(normalize_todo_path("TODO.md"), "TODO.md");
        assert_eq!(normalize_todo_path("./TODO.md"), "TODO.md");
        assert_eq!(normalize_todo_path("notes/../TODO.md"), "TODO.md");
        // Native 键经 resolve_path 锚定：相对/绝对/父目录别名归同一键，
        // 且与 write_file/edit_file 共享 `file:` 命名空间。
        let tool = TodoWriteTool;
        let cwd = Path::new("/w/p");
        let plain = tool
            .exclusive_execution_key(&serde_json::json!({"path": "TODO.md"}), cwd)
            .unwrap();
        for alias in ["./TODO.md", "notes/../TODO.md", "/w/p/TODO.md"] {
            let key = tool
                .exclusive_execution_key(&serde_json::json!({"path": alias}), cwd)
                .unwrap();
            assert_eq!(plain, key, "alias {alias} must share the resource key");
        }
        assert_eq!(plain, "file:/w/p/TODO.md");
    }

    #[tokio::test]
    async fn todo_write_tool_roundtrip_on_fs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = TodoWriteTool;
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: dir.path(),
            metadata: &mut metadata,
        };
        let result = tool
            .execute(serde_json::json!({"item": "ship it"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.output.starts_with("Updated "));
        let content = std::fs::read_to_string(dir.path().join("TODO.md")).unwrap();
        assert_eq!(content, "# TODO\n- [ ] ship it\n");
        // 标记完成
        tool.execute(
            serde_json::json!({"item": "ship it", "checked": true}),
            &mut ctx,
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("TODO.md")).unwrap();
        assert_eq!(content, "# TODO\n- [x] ship it\n");
        // 幂等
        let result = tool
            .execute(
                serde_json::json!({"item": "ship it", "checked": true}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(result.output.starts_with("No change needed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn todo_write_rejects_symlinked_path_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("TODO.md");
        std::fs::write(&target, "# Private TODO\n").unwrap();
        symlink(&target, dir.path().join("TODO.md")).unwrap();
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: dir.path(),
            metadata: &mut metadata,
        };

        let result = TodoWriteTool
            .execute(serde_json::json!({"item": "must stay local"}), &mut ctx)
            .await
            .unwrap();

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("symlink"), "{}", result.output);
        assert_eq!(std::fs::read_to_string(target).unwrap(), "# Private TODO\n");
    }

    #[tokio::test]
    async fn todo_write_rejects_empty_and_multiline_items_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let tool = TodoWriteTool;
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: dir.path(),
            metadata: &mut metadata,
        };
        for item in ["", "   \t", "first\nsecond", "first\r\nsecond"] {
            let result = tool
                .execute(serde_json::json!({"item": item}), &mut ctx)
                .await
                .unwrap();
            assert!(result.is_error, "{item:?} should be rejected");
        }
        assert!(!dir.path().join("TODO.md").exists());
    }

    #[tokio::test]
    async fn todo_write_trims_outer_whitespace_for_stable_matching() {
        let dir = tempfile::tempdir().unwrap();
        let tool = TodoWriteTool;
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: dir.path(),
            metadata: &mut metadata,
        };
        tool.execute(serde_json::json!({"item": "  stable item  "}), &mut ctx)
            .await
            .unwrap();
        let result = tool
            .execute(serde_json::json!({"item": "stable item"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.output.starts_with("No change needed"));
        let content = std::fs::read_to_string(dir.path().join("TODO.md")).unwrap();
        assert_eq!(content, "# TODO\n- [ ] stable item\n");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn todo_write_rejects_oversized_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("TODO.md")).unwrap();
        file.set_len((crate::tools::filesystem::FILE_INPUT_MAX_BYTES + 1) as u64)
            .unwrap();
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: dir.path(),
            metadata: &mut metadata,
        };
        let result = TodoWriteTool
            .execute(serde_json::json!({"item": "safe"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.output.contains("edit limit"), "{}", result.output);
    }

    #[tokio::test]
    async fn todo_write_rejects_oversized_item_and_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: dir.path(),
            metadata: &mut metadata,
        };
        let result = TodoWriteTool
            .execute(
                serde_json::json!({"item": "x".repeat(TODO_DOCUMENT_MAX_BYTES + 1)}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.output.contains("TODO item"), "{}", result.output);
        assert!(!dir.path().join("TODO.md").exists());

        let path = dir.path().join("TODO.md");
        std::fs::write(&path, vec![b'a'; TODO_DOCUMENT_MAX_BYTES]).unwrap();
        let result = TodoWriteTool
            .execute(serde_json::json!({"item": "cannot fit"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.output.contains("would exceed"), "{}", result.output);
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            TODO_DOCUMENT_MAX_BYTES as u64
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn todo_write_resolves_path_like_filesystem_tools() {
        // review 十二轮修复回归：执行路径与权限求值路径同口径。
        // `notes` 目录不存在时，旧实现的字面 `notes/../TODO.md` 会因 OS
        // 路径遍历要求 `notes` 存在而写入失败；词法归一后直接落盘
        // cwd/TODO.md。
        let dir = tempfile::tempdir().unwrap();
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: dir.path(),
            metadata: &mut metadata,
        };
        let result = TodoWriteTool
            .execute(
                serde_json::json!({"item": "aliased", "path": "notes/../TODO.md"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.output);
        let content = std::fs::read_to_string(dir.path().join("TODO.md")).unwrap();
        assert_eq!(content, "# TODO\n- [ ] aliased\n");
        // 与规范路径指向同一文件：幂等命中
        let result = TodoWriteTool
            .execute(
                serde_json::json!({"item": "aliased", "path": "TODO.md"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(result.output.starts_with("No change needed"));
    }

    struct CannedAnswer(&'static str);

    #[async_trait::async_trait]
    impl UserInteraction for CannedAnswer {
        async fn ask(&self, _question: &str) -> String {
            self.0.to_string()
        }
    }

    #[tokio::test]
    async fn ask_user_question_with_and_without_callback() {
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        // 无回调：不可用错误（对齐基线文案）
        let tool = AskUserQuestionTool::new(None);
        let result = tool
            .execute(serde_json::json!({"question": "which?"}), &mut ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(
            result.output,
            "ask_user_question is unavailable in this session"
        );
        // 有回调：答复裁剪空白
        let tool = AskUserQuestionTool::new(Some(Arc::new(CannedAnswer("  option A  "))));
        let result = tool
            .execute(serde_json::json!({"question": "which?"}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(result.output, "option A");
        // 空答复
        let tool = AskUserQuestionTool::new(Some(Arc::new(CannedAnswer("   "))));
        let result = tool
            .execute(serde_json::json!({"question": "which?"}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(result.output, "(no response)");
    }

    #[tokio::test]
    async fn plan_mode_tools_switch_shared_engine() {
        let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let enter = EnterPlanModeTool::new(engine.clone());
        let result = enter.execute(Value::Null, &mut ctx).await.unwrap();
        assert_eq!(result.output, "Permission mode set to plan");
        assert_eq!(engine.mode(), PermissionMode::Plan);
        // plan 模式下写工具被引擎拦截
        assert!(!engine.evaluate("write_file", false, None, None).allowed);

        let exit = ExitPlanModeTool::new(engine.clone());
        let result = exit.execute(Value::Null, &mut ctx).await.unwrap();
        assert_eq!(result.output, "Permission mode set to default");
        assert_eq!(engine.mode(), PermissionMode::Default);
        // enter 只读、exit 非只读（安全语义）
        assert!(enter.is_read_only(&Value::Null));
        assert!(!exit.is_read_only(&Value::Null));
    }
}
