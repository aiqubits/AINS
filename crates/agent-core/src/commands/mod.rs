//! Slash Commands（AINS_PLAN 7+.1）：frontmatter markdown 命令模板，与 Skill
//! 同构互转。
//!
//! 命令是**提示词模板**（不提供新能力）：`/name args` 展开为提交给模型的
//! prompt（可带模型覆盖与工具白名单）。格式与 Skill 一致（YAML frontmatter +
//! Markdown body），故二者可互转——命令即"用户可直接调用的 skill"，skill 即
//! "带元数据的命令模板"。对齐 OpenHarness `commands/registry.py` 的
//! `SlashCommand` + 插件 `PluginCommandDefinition` 的 `_render_*_command_prompt`。
//!
//! 展开规则（对齐基线并扩展位置参数）：
//! - `$ARGUMENTS` / `${ARGUMENTS}` → 原始 args 全文；
//! - `$1`..`$9` / `${1}`..`${9}` → 按空白切分的第 N 个参数（缺失为空）；
//! - 若 args 非空且模板无任何上述占位 → 末尾追加 `Arguments: {args}`。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::CommandError;
use crate::skills::{SkillContent, split_frontmatter};

/// 位置参数占位符（`$1`..`$9` / `${1}`..`${9}`）。
/// regex crate 不支持 look-around：裸 `$N` 以词边界 `\b` 结尾——`$10` 中
/// `$1` 后跟数字（词字符）无边界，保持字面；`${10}` 被长形式排除
/// （review 修复：历史实现逐个 `replace` 会命中 `$10` 中的 `$1`）。
static POSITIONAL_BRACED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([1-9])\}").expect("valid braced positional regex"));
static POSITIONAL_BARE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$([1-9])\b").expect("valid bare positional regex"));

/// 全部占位符的组合正则（**单遍替换**）：长形式优先于裸形式
/// （`${ARGUMENTS}` / `$ARGUMENTS` / `${N}` / `$N`）。
///
/// 单遍替换保证插入的参数字面量（如 args 中的 `$1` / `$ARGUMENTS` 字样）
/// 不会被后续替换遍二次改写（review 修复：多遍 `replace` 会重扫前一遍
/// 插入的文本，参数字面量被破坏）。
static ALL_PLACEHOLDER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{ARGUMENTS\}|\$ARGUMENTS|\$\{([1-9])\}|\$([1-9])\b")
        .expect("valid combined placeholder regex")
});

/// 命令名合法性：非空且不含空白（与 skill command_name 同口径）。
pub(crate) fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_whitespace)
}

/// 命令参数展开上限（64 KiB，字节）。参数来自用户输入（slash 命令调用），
/// 超限展开会生成超大 prompt（token 计费 + 上下文膨胀）。按 UTF-8 字符边界
/// 截断保持有界（与 tasks/personalization 的输出预算同哲学）。
pub const MAX_COMMAND_ARGS_BYTES: usize = 64 * 1024;

/// 命令内部名称不带调用前缀 `/`。接受用户常见的 `/review` 写法并规范化，
/// 使其与 [`CommandRegistry::lookup`] 去前缀后的查找键一致。
fn normalize_name(name: &str) -> &str {
    name.trim().trim_start_matches('/')
}

/// Slash 命令定义（由 frontmatter markdown 解析而来）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommand {
    /// 调用名（`/name`）。
    pub name: String,
    /// 一行描述（frontmatter `description`），用于帮助/列表渲染。
    pub description: String,
    /// 参数提示（frontmatter `argument-hint`），用于帮助/补全展示。
    pub argument_hint: Option<String>,
    /// 声明可用工具（frontmatter `allowed-tools`），随调用结果透出供上层门控。
    pub allowed_tools: Vec<String>,
    /// 模型覆盖（frontmatter `model`），调用时随结果返回。
    pub model: Option<String>,
    /// 提示词模板正文（Markdown body）。
    pub body: String,
}

/// frontmatter 解析视图（连字符键 + 下划线别名兼容）。
#[derive(Debug, Default, Deserialize)]
struct CommandFrontmatter {
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "argument-hint", alias = "argument_hint")]
    argument_hint: Option<String>,
    #[serde(default, rename = "allowed-tools", alias = "allowed_tools")]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    model: Option<String>,
}

/// frontmatter 序列化视图（`to_markdown` 用；空字段省略）。
#[derive(Debug, Serialize)]
struct CommandFrontmatterOut {
    description: String,
    #[serde(rename = "argument-hint", skip_serializing_if = "Option::is_none")]
    argument_hint: Option<String>,
    #[serde(rename = "allowed-tools", skip_serializing_if = "Vec::is_empty")]
    allowed_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

/// 命令调用结果：待提交给模型的 prompt + 可选模型覆盖 + 工具白名单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub prompt: String,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
}

impl SlashCommand {
    /// 从 frontmatter markdown 解析命令。无 frontmatter 时 body 即全文。
    pub fn from_markdown(name: &str, raw: &str) -> Result<Self, CommandError> {
        let name = normalize_name(name);
        if !is_valid_name(name) {
            return Err(CommandError::InvalidFormat(format!(
                "invalid command name: {name:?}"
            )));
        }
        let (fm_raw, body) = split_frontmatter(raw);
        let fm: CommandFrontmatter = if fm_raw.is_empty() {
            CommandFrontmatter::default()
        } else {
            serde_yaml::from_str(&fm_raw)
                .map_err(|e| CommandError::InvalidFormat(format!("frontmatter: {e}")))?
        };
        Ok(Self {
            name: name.to_string(),
            description: fm.description.unwrap_or_default(),
            argument_hint: fm.argument_hint,
            allowed_tools: fm.allowed_tools.unwrap_or_default(),
            model: fm.model,
            body,
        })
    }

    /// 由 Skill 内容构造命令（Skill 的 frontmatter+body 即命令模板）。
    /// description/model 取自 frontmatter；`requires_tools` → `allowed_tools`；
    /// body 为 skill 正文。体现命令与 skill 的同构性。
    pub fn from_skill_content(name: &str, skill: &SkillContent) -> Result<Self, CommandError> {
        let name = normalize_name(name);
        if !is_valid_name(name) {
            return Err(CommandError::InvalidFormat(format!(
                "invalid command name: {name:?}"
            )));
        }
        let fm = &skill.frontmatter;
        let str_field = |key: &str| fm.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let list_field = |key: &str| {
            fm.get(key)
                .and_then(|v| v.as_sequence())
                .map(|seq| {
                    seq.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        // 命令的工具白名单优先取 allowed-tools；Skill frontmatter 同时兼容
        // allowed_tools 下划线写法，最后才回退旧的 requires_tools。
        let mut allowed_tools = list_field("allowed-tools");
        if allowed_tools.is_empty() {
            allowed_tools = list_field("allowed_tools");
        }
        if allowed_tools.is_empty() {
            allowed_tools = list_field("requires_tools");
        }
        Ok(Self {
            name: name.to_string(),
            description: str_field("description").unwrap_or_default(),
            argument_hint: str_field("argument-hint").or_else(|| str_field("argument_hint")),
            allowed_tools,
            model: str_field("model"),
            body: skill.body.clone(),
        })
    }

    /// 序列化回 frontmatter markdown（可作为 Skill 存储，完成同构互转）。
    pub fn to_markdown(&self) -> Result<String, CommandError> {
        let out = CommandFrontmatterOut {
            description: self.description.clone(),
            argument_hint: self.argument_hint.clone(),
            allowed_tools: self.allowed_tools.clone(),
            model: self.model.clone(),
        };
        // serde_yaml 输出以 \n 结尾；包裹为 frontmatter 块 + body。
        let yaml = serde_yaml::to_string(&out)
            .map_err(|e| CommandError::InvalidFormat(format!("serialize: {e}")))?;
        Ok(format!("---\n{yaml}---\n\n{}", self.body))
    }

    /// 模板是否含任一参数占位（`$ARGUMENTS` / `${ARGUMENTS}` / `$1`..`$9` /
    /// `${1}`..`${9}`；`$10` 及以上不算占位，保持字面）。
    fn has_placeholder(&self) -> bool {
        if self.body.contains("$ARGUMENTS") || self.body.contains("${ARGUMENTS}") {
            return true;
        }
        POSITIONAL_BRACED_RE.is_match(&self.body) || POSITIONAL_BARE_RE.is_match(&self.body)
    }

    /// 展开提示词模板（见模块文档的替换规则）。
    ///
    /// 单遍替换（review 修复）：组合正则一次扫描完成位置参数与
    /// `$ARGUMENTS` 全文替换，`$10`/`${10}` 保持字面；插入的参数字面量
    /// （args 中的 `$N` / `$ARGUMENTS` 字样）不被二次改写。
    /// 参数超 [`MAX_COMMAND_ARGS_BYTES`] 时按字符边界截断（防超大 prompt）。
    pub fn expand(&self, args: &str) -> String {
        let raw = args.trim();
        let raw = if raw.len() > MAX_COMMAND_ARGS_BYTES {
            let mut end = MAX_COMMAND_ARGS_BYTES;
            while !raw.is_char_boundary(end) {
                end -= 1;
            }
            &raw[..end]
        } else {
            raw
        };
        let had_placeholder = self.has_placeholder();
        let positional: Vec<&str> = raw.split_whitespace().collect();
        let mut out = ALL_PLACEHOLDER_RE
            .replace_all(&self.body, |caps: &regex::Captures<'_>| {
                if let Some(braced) = caps.get(1) {
                    let idx: usize = braced.as_str().parse().expect("captured digit parses");
                    positional.get(idx - 1).copied().unwrap_or("")
                } else if let Some(bare) = caps.get(2) {
                    let idx: usize = bare.as_str().parse().expect("captured digit parses");
                    positional.get(idx - 1).copied().unwrap_or("")
                } else {
                    raw // $ARGUMENTS / ${ARGUMENTS}
                }
            })
            .into_owned();
        if !raw.is_empty() && !had_placeholder {
            out = format!("{out}\n\nArguments: {raw}");
        }
        out
    }

    /// 调用命令：展开 prompt 并附带模型覆盖与工具白名单。
    pub fn invoke(&self, args: &str) -> CommandOutcome {
        CommandOutcome {
            prompt: self.expand(args),
            model: self.model.clone(),
            allowed_tools: self.allowed_tools.clone(),
        }
    }
}

/// 命令注册表：名称 → 命令，保留注册顺序供列表/帮助渲染。
#[derive(Debug, Default)]
pub struct CommandRegistry {
    commands: HashMap<String, SlashCommand>,
    order: Vec<String>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册命令（同名覆盖，顺序保持首次注册位置）。即使调用方直接构造
    /// `SlashCommand`，也在此处维护“内部名称无 `/` 且合法”的注册表不变量。
    pub fn register(&mut self, mut command: SlashCommand) -> Result<(), CommandError> {
        let name = normalize_name(&command.name);
        if !is_valid_name(name) {
            return Err(CommandError::InvalidFormat(format!(
                "invalid command name: {name:?}"
            )));
        }
        command.name = name.to_string();
        if !self.commands.contains_key(&command.name) {
            self.order.push(command.name.clone());
        }
        self.commands.insert(command.name.clone(), command);
        Ok(())
    }

    /// 从 frontmatter markdown 解析并注册。
    pub fn register_markdown(&mut self, name: &str, raw: &str) -> Result<(), CommandError> {
        self.register(SlashCommand::from_markdown(name, raw)?)
    }

    pub fn get(&self, name: &str) -> Option<&SlashCommand> {
        self.commands.get(name)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// 解析 `/name args`：返回命令与去空白的 args。非 `/` 开头或未注册返回 None。
    /// 与 [`register`](Self::register) 的 `normalize_name` 同口径：容忍
    /// `//name` 等多余前导斜杠（review 修复：历史实现只 strip 一个 `/`，
    /// `//review` 解析出 `name="/review"` 而查不到）。
    pub fn lookup<'a>(&'a self, raw_input: &str) -> Option<(&'a SlashCommand, String)> {
        let rest = raw_input.trim_start_matches('/');
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        self.commands
            .get(name)
            .map(|cmd| (cmd, args.trim().to_string()))
    }

    /// 解析并调用（未命中返回 None）。
    pub fn invoke(&self, raw_input: &str) -> Option<CommandOutcome> {
        self.lookup(raw_input).map(|(cmd, args)| cmd.invoke(&args))
    }

    /// 按注册顺序返回命令。
    pub fn list(&self) -> Vec<&SlashCommand> {
        self.order
            .iter()
            .filter_map(|name| self.commands.get(name))
            .collect()
    }

    /// 帮助文本（Level 0 摘要，供 System Prompt / TUI 展示）。
    pub fn help_text(&self) -> String {
        let mut lines = vec!["Available commands:".to_string()];
        for cmd in self.list() {
            let hint = cmd
                .argument_hint
                .as_deref()
                .map(|h| format!(" {h}"))
                .unwrap_or_default();
            lines.push(format!("/{}{}  {}", cmd.name, hint, cmd.description));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\ndescription: Review a pull request\nargument-hint: \"[pr-number]\"\nallowed-tools:\n  - web_fetch\n  - shell\nmodel: gpt-4o\n---\nReview PR #$ARGUMENTS carefully and summarize risks.";

    #[test]
    fn parse_frontmatter_fields_and_body() {
        let cmd = SlashCommand::from_markdown("review-pr", SAMPLE).unwrap();
        assert_eq!(cmd.name, "review-pr");
        assert_eq!(cmd.description, "Review a pull request");
        assert_eq!(cmd.argument_hint.as_deref(), Some("[pr-number]"));
        assert_eq!(cmd.allowed_tools, vec!["web_fetch", "shell"]);
        assert_eq!(cmd.model.as_deref(), Some("gpt-4o"));
        assert!(cmd.body.starts_with("Review PR #$ARGUMENTS"));
    }

    #[test]
    fn expand_replaces_arguments_placeholder() {
        let cmd = SlashCommand::from_markdown("review-pr", SAMPLE).unwrap();
        let out = cmd.expand("42");
        assert_eq!(out, "Review PR #42 carefully and summarize risks.");
    }

    #[test]
    fn expand_positional_arguments() {
        let cmd = SlashCommand::from_markdown("cmp", "Compare $1 with $2 (${1}).").unwrap();
        assert_eq!(cmd.expand("alpha beta"), "Compare alpha with beta (alpha).");
        // 缺失位置参数替空
        assert_eq!(cmd.expand("alpha"), "Compare alpha with  (alpha).");
    }

    #[test]
    fn expand_does_not_mangle_ten_plus_placeholders() {
        // `$10`/`${10}` 不是位置占位（仅 1..=9）：必须保持字面，
        // 不得被 `$1`/`${1}` 前缀规则部分改写（review 修复回归）。
        let cmd = SlashCommand::from_markdown("pay", "total $10, ${10} ok, $1 here").unwrap();
        assert_eq!(cmd.expand("x"), "total $10, ${10} ok, x here");
        // 仅含 `$10` 的模板不算含占位 → 无占位追加规则仍生效
        let only_ten = SlashCommand::from_markdown("v", "price $10").unwrap();
        assert_eq!(only_ten.expand("99"), "price $10\n\nArguments: 99");
    }

    #[test]
    fn expand_arguments_text_is_not_rewritten_by_positional_pass() {
        // 参数全文替换必须晚于位置替换：args 中的 `$2` 字样不得被
        // 位置占位规则二次改写（review 修复回归）。
        let cmd = SlashCommand::from_markdown("mixed", "use ${ARGUMENTS}; first=$1").unwrap();
        assert_eq!(cmd.expand("echo $2 x"), "use echo $2 x; first=echo");
    }

    #[test]
    fn expand_single_pass_does_not_rewrite_inserted_argument_literals() {
        // 单遍替换回归：多遍 replace 会重扫前一遍插入的文本，参数中的
        // `$1` / `$ARGUMENTS` 字面量会被二次改写（review 修复）。
        let multi = SlashCommand::from_markdown("m", "first=${1} second=${2}").unwrap();
        assert_eq!(multi.expand("a $1 b"), "first=a second=$1");
        let braced_args = SlashCommand::from_markdown("ap", "say ${ARGUMENTS}").unwrap();
        assert_eq!(braced_args.expand("echo $ARGUMENTS"), "say echo $ARGUMENTS");
        // `$10` 仍保持字面（组合正则的 `\b` 边界）。
        let ten = SlashCommand::from_markdown("t", "total $10 + $1").unwrap();
        assert_eq!(ten.expand("x"), "total $10 + x");
    }

    #[test]
    fn expand_appends_args_when_no_placeholder() {
        let cmd = SlashCommand::from_markdown("note", "Take a note.").unwrap();
        assert_eq!(
            cmd.expand("buy milk"),
            "Take a note.\n\nArguments: buy milk"
        );
        // 无 args 不追加
        assert_eq!(cmd.expand("  "), "Take a note.");
    }

    #[test]
    fn no_frontmatter_treats_all_as_body() {
        let cmd = SlashCommand::from_markdown("x", "just a body $ARGUMENTS").unwrap();
        assert_eq!(cmd.description, "");
        assert_eq!(cmd.expand("y"), "just a body y");
    }

    #[test]
    fn invalid_name_rejected() {
        assert!(SlashCommand::from_markdown("has space", "b").is_err());
        assert!(SlashCommand::from_markdown("", "b").is_err());
        assert!(SlashCommand::from_markdown("///", "b").is_err());
    }

    #[test]
    fn lookup_normalizes_double_slash_prefix() {
        // review 修复回归：`//review` 与 `/review` 等价（register 侧
        // normalize_name 已容忍多余前导斜杠，lookup 侧此前不一致）。
        let mut registry = CommandRegistry::new();
        registry
            .register_markdown("/review", "Review $ARGUMENTS")
            .unwrap();
        let (cmd, args) = registry
            .lookup("//review PR-7")
            .expect("//review must resolve");
        assert_eq!(cmd.name, "review");
        assert_eq!(args, "PR-7");
        assert_eq!(
            registry.invoke("//review PR-7").unwrap().prompt,
            "Review PR-7"
        );
        // 纯斜杠不命中（name 为空）。
        assert!(registry.lookup("///").is_none());
    }

    #[test]
    fn leading_slash_name_is_normalized_and_invocable() {
        let mut registry = CommandRegistry::new();
        registry
            .register_markdown("/review", "Review $ARGUMENTS")
            .unwrap();

        assert_eq!(registry.get("review").unwrap().name, "review");
        assert_eq!(registry.invoke("/review 42").unwrap().prompt, "Review 42");
    }

    #[test]
    fn direct_registration_normalizes_and_rejects_invalid_names() {
        let mut registry = CommandRegistry::new();
        registry
            .register(SlashCommand {
                name: "/review".into(),
                description: String::new(),
                argument_hint: None,
                allowed_tools: Vec::new(),
                model: None,
                body: "Review $ARGUMENTS".into(),
            })
            .unwrap();
        assert!(registry.get("review").is_some());
        assert_eq!(registry.invoke("/review 42").unwrap().prompt, "Review 42");

        assert!(
            registry
                .register(SlashCommand {
                    name: "///".into(),
                    description: String::new(),
                    argument_hint: None,
                    allowed_tools: Vec::new(),
                    model: None,
                    body: String::new(),
                })
                .is_err()
        );
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn skill_command_isomorphism_roundtrip() {
        // 命令 → markdown → 解析回命令，字段不丢（同构互转）。
        let cmd = SlashCommand::from_markdown("review-pr", SAMPLE).unwrap();
        let md = cmd.to_markdown().unwrap();
        let back = SlashCommand::from_markdown("review-pr", &md).unwrap();
        assert_eq!(back.description, cmd.description);
        assert_eq!(back.argument_hint, cmd.argument_hint);
        assert_eq!(back.allowed_tools, cmd.allowed_tools);
        assert_eq!(back.model, cmd.model);
        assert_eq!(back.body.trim(), cmd.body.trim());
    }

    #[test]
    fn from_skill_content_maps_fields() {
        let frontmatter: serde_yaml::Value = serde_yaml::from_str(
            "description: Summarize a file\nrequires_tools:\n  - read_file\nmodel: claude-3",
        )
        .unwrap();
        let skill = SkillContent {
            frontmatter,
            body: "Summarize ${ARGUMENTS}".to_string(),
        };
        let cmd = SlashCommand::from_skill_content("summarize", &skill).unwrap();
        assert_eq!(cmd.description, "Summarize a file");
        assert_eq!(cmd.allowed_tools, vec!["read_file"]);
        assert_eq!(cmd.model.as_deref(), Some("claude-3"));
        assert_eq!(cmd.expand("notes.md"), "Summarize notes.md");
    }

    #[test]
    fn from_skill_content_accepts_underscore_allowed_tools_alias() {
        let frontmatter: serde_yaml::Value = serde_yaml::from_str(
            "description: Summarize a file\nallowed_tools:\n  - read_file\n  - shell_command",
        )
        .unwrap();
        let skill = SkillContent {
            frontmatter,
            body: "Summarize ${ARGUMENTS}".to_string(),
        };

        let cmd = SlashCommand::from_skill_content("summarize", &skill).unwrap();
        assert_eq!(cmd.allowed_tools, vec!["read_file", "shell_command"]);
    }

    #[test]
    fn registry_lookup_invoke_and_help() {
        let mut reg = CommandRegistry::new();
        reg.register_markdown("review-pr", SAMPLE).unwrap();
        reg.register_markdown("note", "Take a note.").unwrap();

        assert_eq!(reg.len(), 2);
        // 非命令输入
        assert!(reg.lookup("hello world").is_none());
        assert!(reg.lookup("/unknown x").is_none());
        // 命中 + args
        let (cmd, args) = reg.lookup("/review-pr 42").unwrap();
        assert_eq!(cmd.name, "review-pr");
        assert_eq!(args, "42");
        // 直接调用
        let outcome = reg.invoke("/review-pr 42").unwrap();
        assert_eq!(
            outcome.prompt,
            "Review PR #42 carefully and summarize risks."
        );
        assert_eq!(outcome.model.as_deref(), Some("gpt-4o"));
        assert_eq!(outcome.allowed_tools, vec!["web_fetch", "shell"]);
        // 无参数命令
        assert_eq!(reg.invoke("/note").unwrap().prompt, "Take a note.");
        // 帮助文本含两条
        let help = reg.help_text();
        assert!(help.contains("/review-pr"));
        assert!(help.contains("/note"));
    }

    #[test]
    fn list_preserves_registration_order() {
        let mut reg = CommandRegistry::new();
        reg.register_markdown("b", "B").unwrap();
        reg.register_markdown("a", "A").unwrap();
        let names: Vec<&str> = reg.list().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a"]);
    }

    #[test]
    fn expand_caps_oversized_arguments_at_char_boundary() {
        // review 修复回归：超大 args 会生成超大 prompt（token 计费 + 上下文
        // 膨胀）；必须按 UTF-8 字符边界截断，不得切出半个多字节字符。
        let cmd = SlashCommand::from_markdown("big", "Body: $ARGUMENTS").unwrap();
        let huge = "x".repeat(MAX_COMMAND_ARGS_BYTES + 1024);
        let expanded = cmd.expand(&huge);
        assert!(
            expanded.len() <= MAX_COMMAND_ARGS_BYTES + "Body: ".len(),
            "expanded prompt must stay bounded: {}",
            expanded.len()
        );
        // 多字节内容：截断后必须落在字符边界（不得含半个字符 → 非法 UTF-8）。
        let huge_utf8 = "中".repeat(MAX_COMMAND_ARGS_BYTES / 3 + 100);
        let expanded = cmd.expand(&huge_utf8);
        assert!(std::str::from_utf8(expanded.as_bytes()).is_ok());
        // 未超限的常规参数不受影响。
        assert_eq!(cmd.expand("hello world"), "Body: hello world");
    }

    #[test]
    fn bare_positional_word_boundary_behavior_is_fixed() {
        // 固化已知权衡（模块文档 L24-27）：regex 无 look-around，裸 `$N` 以
        // `\b` 词边界结尾——`$10` 中 `$1` 后跟数字（词字符）无边界，保持字面；
        // Unicode 词字符（如 `界`）同样无 `\b` 边界，`$1界` 不替换，而长形式
        // `${1}界` 无边界要求、正常替换。两种写法在同一模板中结果不同是
        // 有意设计：模板作者应优先使用 `${N}` 长形式。
        let cmd = SlashCommand::from_markdown("u", "a=$1界 b=${1}界").unwrap();
        assert_eq!(cmd.expand("x"), "a=$1界 b=x界");
        // `$10` 保持字面（词字符边界防御）；模板无占位 → 追加 Arguments。
        let digits = SlashCommand::from_markdown("d", "$1").unwrap();
        assert_eq!(digits.expand("1 2"), "1");
        let tens = SlashCommand::from_markdown("t", "$10").unwrap();
        assert_eq!(tens.expand("1 2"), "$10\n\nArguments: 1 2");
    }
}
