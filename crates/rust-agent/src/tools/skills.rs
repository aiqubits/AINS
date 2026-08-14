//! Skill 加载工具：将已保存的技能全文按需提供给 Agent。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::Value;

use crate::error::ToolError;
use crate::personalization::contains_secret_material;
use crate::platform::Platform;
use crate::skills::store::validate_skill_create_input;
use crate::skills::{SkillContext, SkillLoader, SkillManage, SkillStore, skill_checksum};
use crate::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

/// Upper bound on how many skill packages may be simultaneously activated in a
/// session.  Beyond this the entire activation map is dropped (fail closed),
/// never evicted entry-by-entry: an eviction would quietly authorize resources
/// from a package that was never `skill`-ed in the current turn.
const MAX_ACTIVATED_SKILLS: usize = 256;

/// Insert into an activation map under the capacity cap.  Once `MAX_ACTIVATED_SKILLS`
/// is reached all entries are cleared so resource reads require re-`skill`-ing the
/// package; the map only ever over-approximates nothing, so this never grants access.
fn record_activation(map: &mut HashMap<String, String>, name: String, checksum: String) {
    if map.len() >= MAX_ACTIVATED_SKILLS {
        map.clear();
    }
    map.insert(name, checksum);
}

/// 将已保存技能按名称加载给 Agent。技能索引只提供摘要；模型命中某项时需经
/// 本工具显式读取完整流程，避免把全部 SKILL.md 常驻注入上下文。
#[derive(Clone, Default)]
pub struct SkillLoadTool {
    store: Arc<RwLock<Option<Arc<SkillStore>>>>,
    availability: Arc<RwLock<Option<SkillAvailability>>>,
    /// Skill name -> checksum of the SKILL.md body that was explicitly loaded.
    /// A package can be replaced under the same name while a session is live;
    /// name-only activation must not authorize resources from that new package.
    activated: Arc<RwLock<HashMap<String, String>>>,
}

/// Loads a text resource referenced by an already-loaded standard Skill.
#[derive(Clone, Default)]
pub struct SkillResourceLoadTool {
    store: Arc<RwLock<Option<Arc<SkillStore>>>>,
    availability: Arc<RwLock<Option<SkillAvailability>>>,
    activated: Arc<RwLock<HashMap<String, String>>>,
}

/// Persists exactly one model-generated skill after an explicit chat-command
/// authorization.  It is deliberately unavailable by default: normal Agent
/// turns neither see its schema nor can execute it.  The host grants one token
/// only after the user has issued `/skill-create <request>`.
#[derive(Clone, Default)]
pub struct SkillCreateTool {
    store: Arc<RwLock<Option<Arc<SkillStore>>>>,
    authorized: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SkillAvailability {
    platform: Platform,
    registered_tools: HashSet<String>,
    disabled_tools: Arc<RwLock<HashSet<String>>>,
}

impl SkillAvailability {
    fn context(&self) -> SkillContext {
        let disabled = self
            .disabled_tools
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        SkillContext {
            platform: self.platform,
            available_tools: self
                .registered_tools
                .iter()
                .filter(|name| !disabled.contains(*name))
                .cloned()
                .collect(),
        }
    }
}

impl SkillLoadTool {
    #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn activated_skills(&self) -> Arc<RwLock<HashMap<String, String>>> {
        Arc::clone(&self.activated)
    }

    pub fn attach(
        &self,
        store: Arc<SkillStore>,
        platform: Platform,
        registered_tools: HashSet<String>,
        disabled_tools: Arc<RwLock<HashSet<String>>>,
    ) {
        *self
            .store
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(store);
        *self
            .availability
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(SkillAvailability {
            platform,
            registered_tools,
            disabled_tools,
        });
    }

    fn attached_store(&self) -> Result<Arc<SkillStore>, ToolError> {
        self.store
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .ok_or_else(|| ToolError::Execution("skill storage unavailable".into()))
    }

    fn current_context(&self) -> Result<SkillContext, ToolError> {
        self.availability
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(SkillAvailability::context)
            .ok_or_else(|| ToolError::Execution("skill availability unavailable".into()))
    }
}

impl SkillResourceLoadTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct the companion resource reader with the activation state from
    /// its `skill` loader.  Resource reads must never make a skill active by
    /// themselves: the model first explicitly loads the instructions.
    pub fn with_activated(activated: Arc<RwLock<HashMap<String, String>>>) -> Self {
        Self {
            activated,
            ..Self::default()
        }
    }

    pub fn attach(
        &self,
        store: Arc<SkillStore>,
        platform: Platform,
        registered_tools: HashSet<String>,
        disabled_tools: Arc<RwLock<HashSet<String>>>,
    ) {
        *self
            .store
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(store);
        *self
            .availability
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(SkillAvailability {
            platform,
            registered_tools,
            disabled_tools,
        });
    }

    fn attached_store(&self) -> Result<Arc<SkillStore>, ToolError> {
        self.store
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .ok_or_else(|| ToolError::Execution("skill storage unavailable".into()))
    }

    fn current_context(&self) -> Result<SkillContext, ToolError> {
        self.availability
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(SkillAvailability::context)
            .ok_or_else(|| ToolError::Execution("skill availability unavailable".into()))
    }
}

impl SkillCreateTool {
    #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach(&self, store: Arc<SkillStore>) {
        *self
            .store
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(store);
    }

    /// Grant one creation attempt for the current user-command turn.
    pub fn authorize_once(&self) {
        self.authorized.store(true, Ordering::Release);
    }

    /// Revoke a command authorization that was not consumed (completion,
    /// interruption, or delivery failure).
    pub fn revoke(&self) {
        self.authorized.store(false, Ordering::Release);
    }

    fn consume_authorization(&self) -> bool {
        self.authorized
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn attached_store(&self) -> Result<Arc<SkillStore>, ToolError> {
        self.store
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .ok_or_else(|| ToolError::Execution("skill storage unavailable".into()))
    }
}

fn required_string(input: &Value, field: &str) -> Result<String, ToolError> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing required string field: {field}")))
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for SkillLoadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "skill".into(),
            description:
                "Load the complete instructions for a listed reusable skill before applying it."
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_available(&self) -> bool {
        // 与 `skill_resource` 一致：存储未挂载时不对外暴露 `skill` schema。
        // 是否被 /tools 禁用由 runtime 在 schema 组装与 dispatch 时统一过滤。
        self.store
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some()
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let name = required_string(&input, "name")?;
        let context = self.current_context()?;
        let content = self
            .attached_store()?
            .load_raw_for_context(&name, &context)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        record_activation(
            &mut self
                .activated
                .write()
                .unwrap_or_else(|poison| poison.into_inner()),
            name,
            skill_checksum(&content),
        );
        Ok(ToolResult::ok(content))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for SkillResourceLoadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "skill_resource".into(),
            description: "Load a UTF-8 resource referenced by an activated skill, such as references/guide.md or scripts/check.sh.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" }, "path": { "type": "string" } },
                "required": ["name", "path"]
            }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_available(&self) -> bool {
        self.store
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some()
            && self.current_context().is_ok_and(|context| {
                // `skill_resource` itself is intentionally unavailable until
                // the store is attached. Requiring its own name here creates
                // a bootstrap cycle while the runtime is assembling schemas.
                context.available_tools.iter().any(|tool| tool == "skill")
            })
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let name = required_string(&input, "name")?;
        let path = required_string(&input, "path")?;
        let context = self.current_context()?;
        if !context.available_tools.iter().any(|tool| tool == "skill") {
            return Err(ToolError::Execution("skill unavailable".into()));
        }
        let store = self.attached_store()?;
        // Re-read the current instruction file before authorizing a resource.
        // If a package was updated, deleted and re-imported, or changed outside
        // AINS after the original `skill` call, its old name-only activation is
        // no longer valid.
        let skill = store
            .load_raw_for_context(&name, &context)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let activated = self
            .activated
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&name)
            .is_some_and(|checksum| checksum == &skill_checksum(&skill));
        if !activated {
            return Err(ToolError::Execution(format!(
                "skill `{name}` has not been loaded in its current version; call `skill` before reading its resources"
            )));
        }
        let content = store
            .load_reference(&name, &path)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(ToolResult::ok(content))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for SkillCreateTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "skill_create".into(),
            description: "Persist one complete SKILL.md generated for an explicitly authorized /skill-create request.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Agent Skills name: 1-64 NFKC-normalized lowercase Unicode letters, digits, and single internal hyphens"
                    },
                    "content": {
                        "type": "string",
                        "description": "Complete Agent Skills SKILL.md: YAML frontmatter with matching name and specific description, followed by Markdown instructions"
                    }
                },
                "required": ["name", "content"]
            }),
        }
    }

    fn is_available(&self) -> bool {
        self.authorized.load(Ordering::Acquire)
            && self
                .store
                .read()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_some()
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // 先校验参数与 store 可用性，再消费一次性授权：模型在 /skill-create
        // 命令轮内若参数缺失/含 secret/store 未装配，可自我修正重试，不白白
        // 消耗授权 token；只有到达创建动作之前才 consume（fail-closed：
        // consume 之后任何失败均需重新授权，不会反复执行创建）。
        let name = required_string(&input, "name")?;
        let content = required_string(&input, "content")?;
        if contains_secret_material(&content) {
            return Ok(ToolResult::err(
                "skill content must not contain credentials or secret material",
            ));
        }
        // 与 store 端 create_skill 同源的完整内容校验（name 格式、frontmatter
        // 一致性、体积上限）在 consume 之前执行：name/内容非法时模型可自我
        // 修正重试而不白白消耗一次性授权 token。
        if let Err(error) = validate_skill_create_input(&name, &content) {
            return Ok(ToolResult::err(format!(
                "invalid skill name or content: {error}"
            )));
        }
        let store = self.attached_store()?;
        if !self.consume_authorization() {
            return Ok(ToolResult::err(
                "skill creation requires an explicit /skill-create command",
            ));
        }
        let summary = store
            .create_skill(&name, &content)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(ToolResult::ok(format!(
            "Created skill `{}`. Tell the user its name and a concise summary.",
            summary.name
        )))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::AgentInternal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_map_caps_and_fails_closed() {
        let mut map = HashMap::new();
        for i in 0..MAX_ACTIVATED_SKILLS {
            record_activation(&mut map, format!("s{i}"), format!("c{i}"));
        }
        assert_eq!(map.len(), MAX_ACTIVATED_SKILLS);

        // Exceeding the cap drops the whole map (fail closed) rather than
        // evicting a single entry; only the new activation survives.
        record_activation(&mut map, "overflow".into(), "c-overflow".into());
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("overflow"), Some(&"c-overflow".to_string()));
    }

    #[test]
    fn create_authorization_is_one_shot_and_revocable() {
        let tool = SkillCreateTool::default();
        // 未授权时 consume 必须失败（fail-closed）。
        assert!(!tool.consume_authorization());
        // 授权后恰好消费一次；第二次调用返回 false，杜绝同一授权重复创建。
        tool.authorize_once();
        assert!(tool.consume_authorization());
        assert!(!tool.consume_authorization());

        // 授权后未消费即 revoke：权限被撤销，后续调用不可用。
        let revoked = SkillCreateTool::default();
        revoked.authorize_once();
        revoked.revoke();
        assert!(!revoked.consume_authorization());
    }

    // native-only：依赖 NativeSkillFiles 与真实文件系统（temp_dir / 目录清理），
    // wasm 无 OPFS 测试环境，整体跳过（wasm 端由 tests/web_skills.rs 覆盖）。
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn create_validation_failure_does_not_consume_authorization() {
        use crate::memory::{InMemoryKvStore, KvStore};
        use crate::skills::SkillFiles;
        use crate::skills::files::NativeSkillFiles;
        use crate::tools::ToolMetadata;

        let root =
            std::env::temp_dir().join(format!("ains-tool-skills-create-{}", std::process::id()));
        let kv: Arc<dyn KvStore> = Arc::new(InMemoryKvStore::default());
        let files: Arc<dyn SkillFiles> = Arc::new(NativeSkillFiles::new(root.clone()).unwrap());
        let store = Arc::new(SkillStore::new(kv, files));
        let tool = SkillCreateTool::default();
        tool.attach(store);
        tool.authorize_once();

        let mut metadata = ToolMetadata::new();
        let cwd = std::path::Path::new("/tmp");
        let mut ctx = ToolContext {
            cwd,
            metadata: &mut metadata,
        };

        // 非法 name（含大写）在预检阶段被拒，不得消耗一次性授权。
        let bad = tool
            .execute(
                serde_json::json!({
                    "name": "MySkill",
                    "content": "---\nname: MySkill\ndescription: t\n---\nbody",
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(
            bad.is_error,
            "invalid skill name must fail before consuming authorization"
        );

        // 授权未被消耗：合法创建依然成功。
        let good = tool
            .execute(
                serde_json::json!({
                    "name": "my-skill",
                    "content": "---\nname: my-skill\ndescription: t\n---\nbody",
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(
            !good.is_error,
            "authorization must survive a failed validation, got: {}",
            good.output
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
