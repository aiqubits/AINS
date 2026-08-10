//! Plugin System（AINS_PLAN 7+.2）：聚合贡献包，把
//! **skills / commands / tools / hooks / MCP** 五个注册面统一注入运行时。
//! 对齐 OpenHarness `plugins/` 的 `LoadedPlugin`（manifest + 五类贡献）。
//!
//! 与基线的有意偏差：Python 插件可动态加载工具代码；AINS 是**编译型双 target**
//! 运行时（wasm 无动态原生加载），故 `tools` 面为**声明式**——引用内置工具或
//! MCP 工具并纳入白名单，而非携带可执行代码。commands 复用
//! [`crate::commands::SlashCommand`]，hooks 复用 [`crate::hooks`] 类型，二者可
//! 直接注入既有注册表；skills / tools / mcp 贡献由 `inject` 汇总，交上层分别接入
//! `SkillStore` / `ToolRuntime` / MCP 子系统。

use serde::{Deserialize, Serialize};

use crate::commands::{CommandRegistry, SlashCommand};
use crate::error::CommandError;
use crate::hooks::{HookDefinition, HookEvent, HookRegistry};

fn default_false() -> bool {
    false
}

/// 插件清单（名称 / 版本 / 描述 / 默认启用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_false")]
    pub enabled_by_default: bool,
}

/// skill 贡献：SKILL.md 原文（frontmatter + body），由上层写入 SkillStore。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSkill {
    pub name: String,
    pub content: String,
}

/// 工具贡献来源（声明式）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginToolSource {
    /// 引用内置工具（纳入白名单 / 启用）。
    Builtin,
    /// 引用某 MCP 服务器暴露的工具。
    Mcp { server: String },
}

/// 工具贡献声明（非可执行代码）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginToolDecl {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub source: PluginToolSource,
}

/// MCP 服务器传输声明。
///
/// **安全提示**：`Stdio` 的 `command` 字段是任意外部命令——插件来源必须
/// 受信（本地配置/签名），接线到 MCP 子系统时不得对不受信插件启用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum PluginMcpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Http {
        url: String,
    },
}

/// MCP 服务器贡献声明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMcpServer {
    pub name: String,
    #[serde(flatten)]
    pub transport: PluginMcpTransport,
}

/// hook 贡献：事件 + 定义（直接注入 [`HookRegistry`]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginHook {
    pub event: HookEvent,
    pub definition: HookDefinition,
}

/// command 贡献规格（frontmatter markdown，解析为 [`SlashCommand`]）。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginCommandSpec {
    pub name: String,
    /// frontmatter markdown 模板全文。
    pub markdown: String,
}

/// 插件规格（可由 JSON 清单反序列化；内联五类贡献，无需文件系统）。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginSpec {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_false")]
    pub enabled_by_default: bool,
    #[serde(default)]
    pub skills: Vec<PluginSkill>,
    #[serde(default)]
    pub commands: Vec<PluginCommandSpec>,
    #[serde(default)]
    pub tools: Vec<PluginToolDecl>,
    #[serde(default)]
    pub hooks: Vec<PluginHook>,
    #[serde(default)]
    pub mcp_servers: Vec<PluginMcpServer>,
}

/// 已加载插件（贡献包）。
#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: PluginManifest,
    pub enabled: bool,
    pub skills: Vec<PluginSkill>,
    pub commands: Vec<SlashCommand>,
    pub tools: Vec<PluginToolDecl>,
    pub hooks: Vec<PluginHook>,
    pub mcp_servers: Vec<PluginMcpServer>,
}

impl Plugin {
    /// 从 JSON 清单构建。
    ///
    /// `enabled_by_default` 仅保留为清单元数据；插件来源本身不能通过清单
    /// 自授权。只有宿主显式传入 `Some(true)`（完成信任/签名校验后）才会启用。
    pub fn from_json(json: &str, enabled_override: Option<bool>) -> Result<Self, CommandError> {
        let spec: PluginSpec = serde_json::from_str(json)
            .map_err(|e| CommandError::InvalidFormat(format!("plugin manifest: {e}")))?;
        Self::from_spec(spec, enabled_override)
    }

    /// 从规格构建：命令 markdown 解析为 [`SlashCommand`]，其余原样携带。
    pub fn from_spec(
        spec: PluginSpec,
        enabled_override: Option<bool>,
    ) -> Result<Self, CommandError> {
        validate_plugin_names(&spec)?;
        let commands = spec
            .commands
            .iter()
            .map(|c| SlashCommand::from_markdown(&c.name, &c.markdown))
            .collect::<Result<Vec<_>, _>>()?;
        let enabled = enabled_override.unwrap_or(false);
        Ok(Self {
            manifest: PluginManifest {
                name: spec.name,
                version: spec.version,
                description: spec.description,
                enabled_by_default: spec.enabled_by_default,
            },
            enabled,
            skills: spec.skills,
            commands,
            tools: spec.tools,
            hooks: spec.hooks,
            mcp_servers: spec.mcp_servers,
        })
    }
}

/// 校验清单中参与注册 / 汇总的名称（与 commands 的 `is_valid_name` 同口径）：
/// 空名 / 含空白名会作为空 key 或冲突条目透出到上层注册面（SkillStore、
/// MCP 子系统等），必须在解析期拒绝（review 修复）。命令名由
/// [`SlashCommand::from_markdown`] 自校验，不在此重复。
fn validate_plugin_names(spec: &PluginSpec) -> Result<(), CommandError> {
    if !crate::commands::is_valid_name(&spec.name) {
        return Err(CommandError::InvalidFormat(format!(
            "plugin name {:?} must be non-empty and contain no whitespace",
            spec.name
        )));
    }
    for skill in &spec.skills {
        if !crate::commands::is_valid_name(&skill.name) {
            return Err(CommandError::InvalidFormat(format!(
                "plugin skill name {:?} must be non-empty and contain no whitespace",
                skill.name
            )));
        }
    }
    for tool in &spec.tools {
        if !crate::commands::is_valid_name(&tool.name) {
            return Err(CommandError::InvalidFormat(format!(
                "plugin tool name {:?} must be non-empty and contain no whitespace",
                tool.name
            )));
        }
        if let PluginToolSource::Mcp { server } = &tool.source
            && !crate::commands::is_valid_name(server)
        {
            return Err(CommandError::InvalidFormat(format!(
                "plugin tool mcp server {:?} must be non-empty and contain no whitespace",
                server
            )));
        }
    }
    for mcp in &spec.mcp_servers {
        if !crate::commands::is_valid_name(&mcp.name) {
            return Err(CommandError::InvalidFormat(format!(
                "plugin mcp server name {:?} must be non-empty and contain no whitespace",
                mcp.name
            )));
        }
    }
    Ok(())
}

/// 五注册面统一注入的汇总：命令 / hooks 已直接注入既有注册表；
/// skills / tools / mcp 贡献在此汇总，交上层接入各自子系统。
#[derive(Debug, Default)]
pub struct InjectionSummary {
    pub commands_injected: usize,
    pub hooks_injected: usize,
    pub skills: Vec<PluginSkill>,
    pub tools: Vec<PluginToolDecl>,
    pub mcp_servers: Vec<PluginMcpServer>,
}

/// 插件注册表：聚合多个插件并统一注入。
#[derive(Debug, Default)]
pub struct PluginRegistry {
    plugins: Vec<Plugin>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Plugin) {
        // 同名插件替换（last-wins，与命令面覆盖语义一致）：避免重复注册
        // 同名插件后 inject 汇总面（skills/tools/mcp）出现重复条目
        // （review 修复：历史实现 push，同名插件两次注册会双份注入）。
        if let Some(existing) = self
            .plugins
            .iter_mut()
            .find(|p| p.manifest.name == plugin.manifest.name)
        {
            *existing = plugin;
        } else {
            self.plugins.push(plugin);
        }
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// 已启用插件迭代。
    pub fn enabled(&self) -> impl Iterator<Item = &Plugin> {
        self.plugins.iter().filter(|p| p.enabled)
    }

    /// **五注册面统一注入**：把所有已启用插件的 commands / hooks 注入传入的
    /// 注册表，并汇总 skills / tools / mcp 贡献返回。禁用插件完全惰性
    /// （不贡献任何面）。同名 command 后注册覆盖（`CommandRegistry` 语义）；
    /// 汇总面按贡献去重（review 修复：同一 inject 调用内重复贡献只汇总
    /// 一次，宿主重复注入不会得到重复 skill/tool/mcp 条目）。
    pub fn inject(
        &self,
        commands: &mut CommandRegistry,
        hooks: &mut HookRegistry,
    ) -> InjectionSummary {
        let mut summary = InjectionSummary::default();
        for plugin in self.enabled() {
            for cmd in &plugin.commands {
                // Commands may also have been directly constructed by a
                // trusted host. Preserve CommandRegistry's name invariant
                // rather than silently inserting an uninvocable key.
                if commands.register(cmd.clone()).is_err() {
                    continue;
                }
                summary.commands_injected += 1;
            }
            for hook in &plugin.hooks {
                if hooks.register_if_absent(hook.event, hook.definition.clone()) {
                    summary.hooks_injected += 1;
                }
            }
            // 汇总面去重（内容相等即重复）：插件规模极小，线性 contains 足够。
            for skill in &plugin.skills {
                if !summary.skills.contains(skill) {
                    summary.skills.push(skill.clone());
                }
            }
            for tool in &plugin.tools {
                if !summary.tools.contains(tool) {
                    summary.tools.push(tool.clone());
                }
            }
            for mcp in &plugin.mcp_servers {
                if !summary.mcp_servers.contains(mcp) {
                    summary.mcp_servers.push(mcp.clone());
                }
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
        "name": "demo-plugin",
        "version": "1.0.0",
        "description": "A demo bundle",
        "skills": [{"name": "greet", "content": "---\ndescription: Greet\n---\nSay hi"}],
        "commands": [{"name": "hello", "markdown": "---\ndescription: Say hello\n---\nGreet $ARGUMENTS"}],
        "tools": [{"name": "read_file", "description": "read", "source": {"kind": "builtin"}},
                  {"name": "search", "source": {"kind": "mcp", "server": "docs"}}],
        "hooks": [{"event": "pre_tool_use", "definition": {"type": "command", "command": "echo hi"}}],
        "mcp_servers": [{"name": "docs", "transport": "http", "url": "https://mcp.example.com"}]
    }"#;

    #[test]
    fn from_json_parses_all_five_surfaces() {
        let plugin = Plugin::from_json(MANIFEST, None).unwrap();
        assert_eq!(plugin.manifest.name, "demo-plugin");
        assert!(!plugin.enabled); // 未显式信任的清单默认禁用
        assert_eq!(plugin.skills.len(), 1);
        assert_eq!(plugin.commands.len(), 1);
        assert_eq!(plugin.commands[0].name, "hello");
        assert_eq!(plugin.tools.len(), 2);
        assert_eq!(plugin.hooks.len(), 1);
        assert_eq!(plugin.mcp_servers.len(), 1);
        assert!(matches!(
            plugin.mcp_servers[0].transport,
            PluginMcpTransport::Http { .. }
        ));
    }

    #[test]
    fn manifest_cannot_self_enable_without_host_trust() {
        let manifest = r#"{"name":"untrusted","enabled_by_default":true}"#;
        assert!(!Plugin::from_json(manifest, None).unwrap().enabled);
        assert!(Plugin::from_json(manifest, Some(true)).unwrap().enabled);
    }

    #[test]
    fn inject_wires_commands_and_hooks_and_collects_rest() {
        let plugin = Plugin::from_json(MANIFEST, Some(true)).unwrap();
        let mut registry = PluginRegistry::new();
        registry.register(plugin);

        let mut commands = CommandRegistry::new();
        let mut hooks = HookRegistry::new();
        let summary = registry.inject(&mut commands, &mut hooks);

        // command 面已注入 CommandRegistry
        assert_eq!(summary.commands_injected, 1);
        assert_eq!(
            commands.invoke("/hello world").unwrap().prompt,
            "Greet world"
        );
        // hooks 面已注入 HookRegistry
        assert_eq!(summary.hooks_injected, 1);
        assert_eq!(hooks.get(HookEvent::PreToolUse).len(), 1);
        // skills / tools / mcp 面已汇总
        assert_eq!(summary.skills.len(), 1);
        assert_eq!(summary.tools.len(), 2);
        assert_eq!(summary.mcp_servers.len(), 1);
    }

    #[test]
    fn reinjecting_plugin_does_not_duplicate_hooks() {
        let plugin = Plugin::from_json(MANIFEST, Some(true)).unwrap();
        let mut registry = PluginRegistry::new();
        registry.register(plugin);
        let mut commands = CommandRegistry::new();
        let mut hooks = HookRegistry::new();

        assert_eq!(registry.inject(&mut commands, &mut hooks).hooks_injected, 1);
        assert_eq!(registry.inject(&mut commands, &mut hooks).hooks_injected, 0);
        assert_eq!(hooks.get(HookEvent::PreToolUse).len(), 1);
    }

    #[test]
    fn inject_deduplicates_summary_surfaces_across_reinjection() {
        // review 修复回归：同一插件重复 inject（或重复注册）时，汇总面
        // （skills/tools/mcp）不得出现重复条目。
        let plugin = Plugin::from_json(MANIFEST, Some(true)).unwrap();
        let mut registry = PluginRegistry::new();
        registry.register(plugin);
        let mut commands = CommandRegistry::new();
        let mut hooks = HookRegistry::new();

        let first = registry.inject(&mut commands, &mut hooks);
        assert_eq!(first.skills.len(), 1);
        assert_eq!(first.tools.len(), 2);
        assert_eq!(first.mcp_servers.len(), 1);

        let second = registry.inject(&mut commands, &mut hooks);
        assert_eq!(second.skills.len(), 1, "skills 重复注入必须去重");
        assert_eq!(second.tools.len(), 2, "tools 重复注入必须去重");
        assert_eq!(second.mcp_servers.len(), 1, "mcp 重复注入必须去重");
    }

    #[test]
    fn registering_same_name_plugin_replaces_not_duplicates() {
        // review 修复回归：同名插件重复注册按 last-wins 替换，不产生
        // 双份注入（与命令面覆盖语义一致）。
        let mut registry = PluginRegistry::new();
        registry.register(Plugin::from_json(MANIFEST, Some(true)).unwrap());
        assert_eq!(registry.len(), 1);
        registry.register(Plugin::from_json(MANIFEST, Some(true)).unwrap());
        assert_eq!(registry.len(), 1, "同名插件必须替换而非追加");
    }

    #[test]
    fn disabled_plugin_is_inert() {
        let plugin = Plugin::from_json(MANIFEST, Some(false)).unwrap();
        assert!(!plugin.enabled);
        let mut registry = PluginRegistry::new();
        registry.register(plugin);

        let mut commands = CommandRegistry::new();
        let mut hooks = HookRegistry::new();
        let summary = registry.inject(&mut commands, &mut hooks);

        assert_eq!(summary.commands_injected, 0);
        assert_eq!(summary.hooks_injected, 0);
        assert!(summary.skills.is_empty());
        assert!(summary.tools.is_empty());
        assert!(summary.mcp_servers.is_empty());
        assert!(commands.is_empty());
        assert!(hooks.is_empty());
    }

    #[test]
    fn multiple_plugins_aggregate() {
        let mut registry = PluginRegistry::new();
        registry.register(Plugin::from_json(MANIFEST, Some(true)).unwrap());
        registry.register(
            Plugin::from_json(
                r#"{"name":"p2","commands":[{"name":"bye","markdown":"Bye"}]}"#,
                Some(true),
            )
            .unwrap(),
        );
        assert_eq!(registry.len(), 2);

        let mut commands = CommandRegistry::new();
        let mut hooks = HookRegistry::new();
        let summary = registry.inject(&mut commands, &mut hooks);
        assert_eq!(summary.commands_injected, 2);
        assert!(commands.get("hello").is_some());
        assert!(commands.get("bye").is_some());
    }

    #[test]
    fn invalid_command_markdown_rejects_plugin() {
        let bad = r#"{"name":"bad","commands":[{"name":"has space","markdown":"x"}]}"#;
        assert!(Plugin::from_json(bad, None).is_err());
    }

    #[test]
    fn tool_source_variants_deserialize() {
        let plugin = Plugin::from_json(MANIFEST, None).unwrap();
        assert_eq!(plugin.tools[0].source, PluginToolSource::Builtin);
        assert_eq!(
            plugin.tools[1].source,
            PluginToolSource::Mcp {
                server: "docs".into()
            }
        );
    }

    #[test]
    fn invalid_registration_names_reject_plugin() {
        // review 修复回归：空名 / 含空白名会作为空 key 或冲突条目透出到
        // 上层注册面（SkillStore / MCP 子系统），必须在解析期拒绝。
        let bad_names = [
            r#"{"name":""}"#,
            r#"{"name":"has space"}"#,
            r#"{"name":"ok","skills":[{"name":"","content":"x"}]}"#,
            r#"{"name":"ok","skills":[{"name":"bad skill","content":"x"}]}"#,
            r#"{"name":"ok","tools":[{"name":"","description":"d","source":{"kind":"builtin"}}]}"#,
            r#"{"name":"ok","tools":[{"name":"t","description":"d","source":{"kind":"mcp","server":""}}]}"#,
            r#"{"name":"ok","mcp_servers":[{"name":"bad server","transport":"http","url":"https://x"}]}"#,
        ];
        for manifest in bad_names {
            let error = Plugin::from_json(manifest, None).unwrap_err();
            assert!(
                matches!(error, CommandError::InvalidFormat(_)),
                "{manifest} must be rejected: {error:?}"
            );
        }
        // 合法清单不受影响。
        assert!(Plugin::from_json(MANIFEST, None).is_ok());
    }
}
