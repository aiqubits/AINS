//! Pure Rust 计算类工具（Calculator / JSON / Text / Markdown / Date，双 target）。
//!
//! 基线无对应物（OpenHarness 无此五件套），按 AINS_PLAN 3.2 设计：
//! 全部只读、无平台依赖，WASM 与 Native 行为一致。

use serde_json::Value;

use crate::error::ToolError;
use crate::memory::kv::now_ms;
use crate::memory::memdir::{format_iso_utc, parse_iso_utc};
use crate::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

/// 四位年份 ISO-8601 表示可覆盖的 epoch 毫秒边界。
const ISO_UTC_MIN_EPOCH_MS: i64 = -62_167_219_200_000; // 0000-01-01T00:00:00Z
const ISO_UTC_MAX_EPOCH_MS: i64 = 253_402_300_799_999; // 9999-12-31T23:59:59.999Z
/// Pure transformation tools construct their result before the shared
/// inline/artifact budget runs, so they also need allocation-time bounds.
pub const COMPUTE_INPUT_MAX_BYTES: usize = 1024 * 1024;
pub const COMPUTE_OUTPUT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// epoch 毫秒 → 四位年 UTC ISO-8601。记忆系统的 `format_iso_utc`
/// 有意保持秒级粒度；Date Tool 的 `from_epoch_ms` 则必须保留非零毫秒，
/// 才能与 `to_epoch_ms` 无损往返。
fn format_iso_utc_millis(ms: i64) -> String {
    let seconds = format_iso_utc(ms);
    let fractional_ms = ms.rem_euclid(1000);
    if fractional_ms == 0 {
        seconds
    } else {
        format!("{}.{fractional_ms:03}Z", seconds.trim_end_matches('Z'))
    }
}

fn require_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    let value = input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing required string field: {key}")))?;
    validate_compute_input(key, value)?;
    Ok(value)
}

fn optional_str<'a>(input: &'a Value, key: &str, default: &'a str) -> Result<&'a str, ToolError> {
    let value = input.get(key).and_then(Value::as_str).unwrap_or(default);
    validate_compute_input(key, value)?;
    Ok(value)
}

fn validate_compute_input(key: &str, value: &str) -> Result<(), ToolError> {
    if value.len() > COMPUTE_INPUT_MAX_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "string field '{key}' exceeds the {COMPUTE_INPUT_MAX_BYTES} byte compute input limit"
        )));
    }
    Ok(())
}

fn bounded_compute_output(output: String) -> ToolResult {
    if output.len() > COMPUTE_OUTPUT_MAX_BYTES {
        ToolResult::err(format!(
            "compute result would exceed the {COMPUTE_OUTPUT_MAX_BYTES} byte output limit"
        ))
    } else {
        ToolResult::ok(output)
    }
}

/// 分配时即拒绝超限输出的有界写入器：放大类变换（JSON pretty、
/// Markdown 嵌套 HTML）必须在序列化过程中被截断，而不是先分配
/// 完整结果再事后检查。
struct LimitedBuffer {
    bytes: Vec<u8>,
}

impl std::io::Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
            return Err(std::io::Error::other("compute output size overflow"));
        };
        if next_len > COMPUTE_OUTPUT_MAX_BYTES {
            return Err(std::io::Error::other("compute output limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Markdown → HTML，经有界写入器渲染：深度嵌套 blockquote 等构造
/// 可把 1 MiB 入参放大到数十 MiB HTML，必须在 8 MiB 处就地拒绝。
fn render_markdown_html(markdown: &str) -> Result<String, ToolError> {
    let mut buffer = LimitedBuffer { bytes: Vec::new() };
    pulldown_cmark::html::write_html(&mut buffer, pulldown_cmark::Parser::new(markdown)).map_err(
        |_| {
            ToolError::InvalidInput(format!(
                "HTML result would exceed the {COMPUTE_OUTPUT_MAX_BYTES} byte output limit"
            ))
        },
    )?;
    String::from_utf8(buffer.bytes)
        .map(|html| html.trim_end().to_string())
        .map_err(|_| ToolError::InvalidInput("HTML result is not valid UTF-8".into()))
}

fn serialize_json_output(value: &Value, pretty: bool) -> Result<String, ToolError> {
    let mut buffer = LimitedBuffer { bytes: Vec::new() };
    let serialized = if pretty {
        serde_json::to_writer_pretty(&mut buffer, value)
    } else {
        serde_json::to_writer(&mut buffer, value)
    };
    serialized.map_err(|error| {
        ToolError::InvalidInput(format!(
            "JSON result would exceed the {COMPUTE_OUTPUT_MAX_BYTES} byte output limit: {error}"
        ))
    })?;
    String::from_utf8(buffer.bytes).map_err(|error| ToolError::Execution(error.to_string()))
}

fn replacement_output_bytes(text: &str, from: &str, to: &str) -> Result<usize, ToolError> {
    let replacements = text.matches(from).count();
    let removed = replacements
        .checked_mul(from.len())
        .ok_or_else(|| ToolError::InvalidInput("text replacement size overflow".into()))?;
    let added = replacements
        .checked_mul(to.len())
        .ok_or_else(|| ToolError::InvalidInput("text replacement size overflow".into()))?;
    text.len()
        .checked_sub(removed)
        .and_then(|size| size.checked_add(added))
        .ok_or_else(|| ToolError::InvalidInput("text replacement size overflow".into()))
}

// ── Calculator ──────────────────────────────────────────────────────────

/// 四则运算计算器：`+ - * / % ^`、括号、一元负号（Shunting-yard，f64）。
pub struct CalculatorTool;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for CalculatorTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "calculator".into(),
            description: "Evaluate an arithmetic expression (+ - * / % ^, parentheses). \
                          Unary minus binds tighter than '^': -2 ^ 2 = 4; write -(2 ^ 2) for -4."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "expression": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES,
                                   "description": "Arithmetic expression"}
                },
                "required": ["expression"]
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
        let expression = require_str(&input, "expression")?;
        match eval_expression(expression) {
            Ok(value) => Ok(ToolResult::ok(format_number(value))),
            Err(reason) => Ok(ToolResult::err(format!(
                "invalid expression '{expression}': {reason}"
            ))),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CalcToken {
    Number(f64),
    Op(char),
    LeftParen,
    RightParen,
}

fn tokenize(expression: &str) -> Result<Vec<CalcToken>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = expression.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '0'..='9' | '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let literal: String = chars[start..i].iter().collect();
                let value = literal
                    .parse::<f64>()
                    .map_err(|_| format!("bad number literal '{literal}'"))?;
                tokens.push(CalcToken::Number(value));
            }
            '+' | '-' | '*' | '/' | '%' | '^' => {
                // 一元负号：前一 token 不是数字/右括号时，`-` 为前缀取负。
                // 独立算符 `~`（优先级高于 `^`、右结合），不得展开为裸
                // `0 -`（review 二轮修复：旧展开法使 `2^-3`/`2*-3` 按二元
                // 减法低优先级结合，静默算错）。
                let unary = c == '-'
                    && !matches!(
                        tokens.last(),
                        Some(CalcToken::Number(_)) | Some(CalcToken::RightParen)
                    );
                tokens.push(CalcToken::Op(if unary { '~' } else { c }));
                i += 1;
            }
            '(' => {
                tokens.push(CalcToken::LeftParen);
                i += 1;
            }
            ')' => {
                tokens.push(CalcToken::RightParen);
                i += 1;
            }
            other => return Err(format!("unexpected character '{other}'")),
        }
    }
    Ok(tokens)
}

fn precedence(op: char) -> u8 {
    match op {
        '+' | '-' => 1,
        '*' | '/' | '%' => 2,
        '^' => 3,
        // 一元负号：绑定最紧（`-2^2 = (-2)^2 = 4`，取负作用于紧随操作数）
        '~' => 4,
        _ => 0,
    }
}

/// Shunting-yard → RPN → 求值。`^` 与一元 `~` 右结合，其余左结合。
fn eval_expression(expression: &str) -> Result<f64, String> {
    let tokens = tokenize(expression)?;
    if tokens.is_empty() {
        return Err("empty expression".into());
    }
    let mut output: Vec<CalcToken> = Vec::new();
    let mut ops: Vec<CalcToken> = Vec::new();
    for token in tokens {
        match token {
            CalcToken::Number(_) => output.push(token),
            CalcToken::Op(op) => {
                while let Some(CalcToken::Op(top)) = ops.last() {
                    let should_pop = precedence(*top) > precedence(op)
                        || (precedence(*top) == precedence(op) && op != '^' && op != '~');
                    if should_pop {
                        output.push(ops.pop().expect("checked"));
                    } else {
                        break;
                    }
                }
                ops.push(token);
            }
            CalcToken::LeftParen => ops.push(token),
            CalcToken::RightParen => loop {
                match ops.pop() {
                    Some(CalcToken::LeftParen) => break,
                    Some(op) => output.push(op),
                    None => return Err("unbalanced parentheses".into()),
                }
            },
        }
    }
    while let Some(op) = ops.pop() {
        if op == CalcToken::LeftParen {
            return Err("unbalanced parentheses".into());
        }
        output.push(op);
    }

    let mut stack: Vec<f64> = Vec::new();
    for token in output {
        match token {
            CalcToken::Number(value) => stack.push(value),
            // 一元取负：弹单操作数
            CalcToken::Op('~') => {
                let operand = stack.pop().ok_or("malformed expression")?;
                stack.push(-operand);
            }
            CalcToken::Op(op) => {
                let rhs = stack.pop().ok_or("malformed expression")?;
                let lhs = stack.pop().ok_or("malformed expression")?;
                let value = match op {
                    '+' => lhs + rhs,
                    '-' => lhs - rhs,
                    '*' => lhs * rhs,
                    '/' => {
                        if rhs == 0.0 {
                            return Err("division by zero".into());
                        }
                        lhs / rhs
                    }
                    '%' => {
                        if rhs == 0.0 {
                            return Err("division by zero".into());
                        }
                        lhs % rhs
                    }
                    '^' => lhs.powf(rhs),
                    _ => unreachable!(),
                };
                stack.push(value);
            }
            _ => return Err("malformed expression".into()),
        }
    }
    if stack.len() != 1 {
        return Err("malformed expression".into());
    }
    let value = stack[0];
    if !value.is_finite() {
        return Err("result is not finite".into());
    }
    Ok(value)
}

// ── JSON ────────────────────────────────────────────────────────────────

/// JSON 处理：validate / format / minify / get（RFC 6901 pointer）/ keys。
pub struct JsonTool;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for JsonTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "json".into(),
            description: "Inspect or transform a JSON document: validate, format, minify, \
                          get (JSON pointer), keys."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["validate", "format", "minify", "get", "keys"]},
                    "json": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES,
                             "description": "JSON document text"},
                    "pointer": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES,
                                "description": "RFC 6901 JSON pointer (for get/keys)"}
                },
                "required": ["action", "json"]
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
        let action = require_str(&input, "action")?;
        let text = require_str(&input, "json")?;
        let parsed: Value = match serde_json::from_str(text) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Ok(if action == "validate" {
                    ToolResult::ok(format!("invalid: {error}"))
                } else {
                    ToolResult::err(format!("invalid JSON: {error}"))
                });
            }
        };
        let pointer = optional_str(&input, "pointer", "")?;
        let output = match action {
            "validate" => "valid".to_string(),
            "format" => serialize_json_output(&parsed, true)?,
            "minify" => serialize_json_output(&parsed, false)?,
            "get" => match parsed.pointer(pointer) {
                Some(found) => serialize_json_output(found, true)?,
                None => return Ok(ToolResult::err(format!("pointer not found: {pointer}"))),
            },
            "keys" => {
                let target = parsed.pointer(pointer).ok_or_else(|| {
                    ToolError::InvalidInput(format!("pointer not found: {pointer}"))
                })?;
                match target {
                    Value::Object(map) => map.keys().cloned().collect::<Vec<_>>().join("\n"),
                    Value::Array(items) => format!("(array of {} items)", items.len()),
                    other => format!("(scalar: {other})"),
                }
            }
            other => {
                return Err(ToolError::InvalidInput(format!("unknown action: {other}")));
            }
        };
        Ok(bounded_compute_output(output))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

// ── Text ────────────────────────────────────────────────────────────────

/// 文本处理：大小写 / trim / replace / 行列统计。
pub struct TextTool;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for TextTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "text".into(),
            description: "Transform or measure text: upper, lower, trim, replace, count \
                          (chars/words/lines)."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["upper", "lower", "trim", "replace", "count"]},
                    "text": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES},
                    "from": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES,
                             "description": "search text (for replace)"},
                    "to": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES,
                           "description": "replacement text (for replace)"}
                },
                "required": ["action", "text"]
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
        let action = require_str(&input, "action")?;
        let text = require_str(&input, "text")?;
        let output = match action {
            "upper" => text.to_uppercase(),
            "lower" => text.to_lowercase(),
            "trim" => text.trim().to_string(),
            "replace" => {
                let from = require_str(&input, "from")?;
                let to = optional_str(&input, "to", "")?;
                if from.is_empty() {
                    return Err(ToolError::InvalidInput("'from' must not be empty".into()));
                }
                let output_bytes = replacement_output_bytes(text, from, to)?;
                if output_bytes > COMPUTE_OUTPUT_MAX_BYTES {
                    return Ok(ToolResult::err(format!(
                        "text replacement would exceed the {COMPUTE_OUTPUT_MAX_BYTES} byte output limit"
                    )));
                }
                text.replace(from, to)
            }
            "count" => {
                let chars = text.chars().count();
                let words = text.split_whitespace().count();
                let lines = if text.is_empty() {
                    0
                } else {
                    text.lines().count()
                };
                format!("chars: {chars}\nwords: {words}\nlines: {lines}")
            }
            other => {
                return Err(ToolError::InvalidInput(format!("unknown action: {other}")));
            }
        };
        Ok(bounded_compute_output(output))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

// ── Markdown ────────────────────────────────────────────────────────────

/// Markdown 处理：to_html / headings（提纲）/ to_text（剥离标记）。
pub struct MarkdownTool;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for MarkdownTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "markdown".into(),
            description: "Process a Markdown document: to_html, headings (outline), to_text."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["to_html", "headings", "to_text"]},
                    "markdown": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES}
                },
                "required": ["action", "markdown"]
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
        use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

        let action = require_str(&input, "action")?;
        let markdown = require_str(&input, "markdown")?;
        let output = match action {
            "to_html" => render_markdown_html(markdown)?,
            "headings" => {
                let mut lines: Vec<String> = Vec::new();
                let mut current: Option<(HeadingLevel, String)> = None;
                for event in Parser::new(markdown) {
                    match event {
                        Event::Start(Tag::Heading { level, .. }) => {
                            current = Some((level, String::new()));
                        }
                        Event::Text(text) | Event::Code(text) => {
                            if let Some((_, buffer)) = current.as_mut() {
                                buffer.push_str(&text);
                            }
                        }
                        Event::End(TagEnd::Heading(_)) => {
                            if let Some((level, title)) = current.take() {
                                let depth = level as usize; // H1=1 … H6=6
                                lines.push(format!(
                                    "{}- {}",
                                    "  ".repeat(depth.saturating_sub(1)),
                                    title.trim()
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                if lines.is_empty() {
                    "(no headings)".to_string()
                } else {
                    lines.join("\n")
                }
            }
            "to_text" => {
                let mut text = String::new();
                for event in Parser::new(markdown) {
                    match event {
                        Event::Text(chunk) | Event::Code(chunk) => text.push_str(&chunk),
                        Event::SoftBreak | Event::HardBreak => text.push('\n'),
                        Event::End(TagEnd::Paragraph)
                        | Event::End(TagEnd::Heading(_))
                        | Event::End(TagEnd::Item)
                        | Event::End(TagEnd::CodeBlock) => text.push('\n'),
                        _ => {}
                    }
                }
                text.trim().to_string()
            }
            other => {
                return Err(ToolError::InvalidInput(format!("unknown action: {other}")));
            }
        };
        Ok(bounded_compute_output(output))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

// ── Date ────────────────────────────────────────────────────────────────

/// 日期时间：now / from_epoch_ms / to_epoch_ms / diff_days（ISO-8601 UTC，
/// 复用 memdir 的 civil 历法换算，无 chrono 依赖）。
pub struct DateTool;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for DateTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "date".into(),
            description: "Date/time utilities in UTC: now, from_epoch_ms, to_epoch_ms, \
                          diff_days between two ISO-8601 timestamps."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["now", "from_epoch_ms", "to_epoch_ms", "diff_days"]},
                    "epoch_ms": {"type": "integer"},
                    "iso": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES,
                            "description": "ISO-8601 UTC timestamp (for to_epoch_ms)"},
                    "from_iso": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES,
                                 "description": "start timestamp (for diff_days)"},
                    "to_iso": {"type": "string", "maxLength": COMPUTE_INPUT_MAX_BYTES,
                               "description": "end timestamp (for diff_days)"}
                },
                "required": ["action"]
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
        let action = require_str(&input, "action")?;
        let output = match action {
            "now" => {
                let ms = now_ms();
                format!("{} (epoch_ms: {ms})", format_iso_utc(ms))
            }
            "from_epoch_ms" => {
                let ms = input
                    .get("epoch_ms")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required integer field: epoch_ms".into())
                    })?;
                if !(ISO_UTC_MIN_EPOCH_MS..=ISO_UTC_MAX_EPOCH_MS).contains(&ms) {
                    return Err(ToolError::InvalidInput(
                        "epoch_ms is outside the supported four-digit-year ISO-8601 range".into(),
                    ));
                }
                format_iso_utc_millis(ms)
            }
            "to_epoch_ms" => {
                let iso = require_str(&input, "iso")?;
                let ms = parse_iso_utc(iso).ok_or_else(|| {
                    ToolError::InvalidInput(format!("invalid ISO-8601 UTC timestamp: {iso}"))
                })?;
                ms.to_string()
            }
            "diff_days" => {
                let from = require_str(&input, "from_iso")?;
                let to = require_str(&input, "to_iso")?;
                let from_ms = parse_iso_utc(from).ok_or_else(|| {
                    ToolError::InvalidInput(format!("invalid ISO-8601 UTC timestamp: {from}"))
                })?;
                let to_ms = parse_iso_utc(to).ok_or_else(|| {
                    ToolError::InvalidInput(format!("invalid ISO-8601 UTC timestamp: {to}"))
                })?;
                const DAY_MS: i64 = 24 * 60 * 60 * 1000;
                // 向负无穷取整（div_euclid）：负区间符合日历直觉
                // （差 -0.5 天计 -1 天，同 Python `//`）
                let difference = to_ms.checked_sub(from_ms).ok_or_else(|| {
                    ToolError::InvalidInput("date difference is outside the supported range".into())
                })?;
                difference.div_euclid(DAY_MS).to_string()
            }
            other => {
                return Err(ToolError::InvalidInput(format!("unknown action: {other}")));
            }
        };
        Ok(bounded_compute_output(output))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

/// 注册全部计算类工具（宿主便捷入口）。
pub fn register_compute_tools(runtime: &mut crate::tools::ToolRuntime) {
    runtime.register(Box::new(CalculatorTool));
    runtime.register(Box::new(JsonTool));
    runtime.register(Box::new(TextTool));
    runtime.register(Box::new(MarkdownTool));
    runtime.register(Box::new(DateTool));
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::tools::ToolMetadata;
    use std::path::Path;

    async fn run(tool: &dyn Tool, input: Value) -> ToolResult {
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        tool.execute(input, &mut ctx).await.unwrap()
    }

    #[test]
    fn calculator_expression_evaluation() {
        assert_eq!(eval_expression("1 + 2 * 3").unwrap(), 7.0);
        assert_eq!(eval_expression("(1 + 2) * 3").unwrap(), 9.0);
        assert_eq!(eval_expression("-4 + 2").unwrap(), -2.0);
        assert_eq!(eval_expression("2 ^ 3 ^ 2").unwrap(), 512.0); // 右结合
        assert_eq!(eval_expression("10 % 3").unwrap(), 1.0);
        assert_eq!(eval_expression("7 / 2").unwrap(), 3.5);
        assert_eq!(eval_expression("-(2 + 3)").unwrap(), -5.0);
        assert!(eval_expression("1 / 0").is_err());
        assert!(eval_expression("(1 + 2").is_err());
        assert!(eval_expression("1 +").is_err());
        assert!(eval_expression("abc").is_err());
        assert!(eval_expression("").is_err());
        assert!(eval_expression("()").is_err());
        assert!(eval_expression("1 2").is_err());
    }

    #[test]
    fn calculator_unary_minus_precedence_matrix() {
        // review 二轮修复回归：一元负号为独立高优先级右结合算符，
        // 旧 "0 -" 展开法在高优先级邻接下静默算错
        assert_eq!(eval_expression("2 ^ -3").unwrap(), 0.125);
        assert_eq!(eval_expression("2 * -3").unwrap(), -6.0);
        assert_eq!(eval_expression("5 - -3").unwrap(), 8.0);
        assert_eq!(eval_expression("--5").unwrap(), 5.0);
        assert_eq!(eval_expression("-2 * -3").unwrap(), 6.0);
        // 取负绑定紧随操作数：(-2)^2 = 4（工具约定，非 Python ** 口径）
        assert_eq!(eval_expression("-2 ^ 2").unwrap(), 4.0);
        assert!(eval_expression("-").is_err());
        assert!(eval_expression("2 ^ -").is_err());
    }

    #[tokio::test]
    async fn calculator_tool_formats_integers() {
        let result = run(&CalculatorTool, serde_json::json!({"expression": "2 + 2"})).await;
        assert_eq!(result.output, "4");
        let result = run(&CalculatorTool, serde_json::json!({"expression": "7/2"})).await;
        assert_eq!(result.output, "3.5");
        let result = run(&CalculatorTool, serde_json::json!({"expression": "1/0"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn json_tool_actions() {
        let doc = r#"{"a": {"b": [1, 2]}, "c": "x"}"#;
        let result = run(
            &JsonTool,
            serde_json::json!({"action": "validate", "json": doc}),
        )
        .await;
        assert_eq!(result.output, "valid");
        let result = run(
            &JsonTool,
            serde_json::json!({"action": "validate", "json": "{oops"}),
        )
        .await;
        assert!(result.output.starts_with("invalid:"));
        let result = run(
            &JsonTool,
            serde_json::json!({"action": "get", "json": doc, "pointer": "/a/b/1"}),
        )
        .await;
        assert_eq!(result.output, "2");
        let result = run(
            &JsonTool,
            serde_json::json!({"action": "keys", "json": doc}),
        )
        .await;
        assert_eq!(result.output, "a\nc");
        let result = run(
            &JsonTool,
            serde_json::json!({"action": "minify", "json": "{ \"a\" : 1 }"}),
        )
        .await;
        assert_eq!(result.output, r#"{"a":1}"#);
        let result = run(
            &JsonTool,
            serde_json::json!({"action": "get", "json": doc, "pointer": "/missing"}),
        )
        .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn text_tool_actions() {
        let result = run(
            &TextTool,
            serde_json::json!({"action": "upper", "text": "abc"}),
        )
        .await;
        assert_eq!(result.output, "ABC");
        let result = run(
            &TextTool,
            serde_json::json!({"action": "replace", "text": "a-b-c", "from": "-", "to": "+"}),
        )
        .await;
        assert_eq!(result.output, "a+b+c");
        let result = run(
            &TextTool,
            serde_json::json!({"action": "count", "text": "你好 world\nsecond line"}),
        )
        .await;
        assert!(result.output.contains("chars: 20"), "{}", result.output);
        assert!(result.output.contains("words: 4"));
        assert!(result.output.contains("lines: 2"));
    }

    #[tokio::test]
    async fn text_replace_rejects_amplified_output_before_allocation() {
        let text = "a".repeat(1024);
        let replacement = "b".repeat(COMPUTE_OUTPUT_MAX_BYTES / text.len() + 1);
        let result = run(
            &TextTool,
            serde_json::json!({
                "action": "replace",
                "text": text,
                "from": "a",
                "to": replacement
            }),
        )
        .await;
        assert!(result.is_error);
        assert!(result.output.contains("text replacement would exceed"));

        assert_eq!(replacement_output_bytes("éé", "é", "界").unwrap(), 6);
    }

    #[tokio::test]
    async fn compute_tools_reject_oversized_string_input() {
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let oversized = "x".repeat(COMPUTE_INPUT_MAX_BYTES + 1);
        let result = TextTool
            .execute(
                serde_json::json!({"action": "count", "text": oversized}),
                &mut ctx,
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(message)) if message.contains("compute input limit"))
        );
    }

    #[tokio::test]
    async fn json_format_stops_when_pretty_output_exceeds_limit() {
        // A compact, valid input can become much larger solely because every
        // element inherits the indentation of deeply nested parent arrays.
        let mut value = Value::Array(vec![Value::Null; 50_000]);
        for _ in 0..100 {
            value = Value::Array(vec![value]);
        }
        let compact = serde_json::to_string(&value).unwrap();
        assert!(compact.len() < COMPUTE_INPUT_MAX_BYTES);

        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = JsonTool
            .execute(
                serde_json::json!({"action": "format", "json": compact}),
                &mut ctx,
            )
            .await;

        assert!(
            matches!(result, Err(ToolError::InvalidInput(message)) if message.contains("JSON result would exceed"))
        );
    }

    #[tokio::test]
    async fn markdown_tool_actions() {
        let markdown = "# Title\n\nSome *text*.\n\n## Section A\n\n- item";
        let result = run(
            &MarkdownTool,
            serde_json::json!({"action": "to_html", "markdown": markdown}),
        )
        .await;
        assert!(result.output.contains("<h1>Title</h1>"));
        let result = run(
            &MarkdownTool,
            serde_json::json!({"action": "headings", "markdown": markdown}),
        )
        .await;
        assert_eq!(result.output, "- Title\n  - Section A");
        let result = run(
            &MarkdownTool,
            serde_json::json!({"action": "to_text", "markdown": markdown}),
        )
        .await;
        assert!(result.output.contains("Some text."));
        assert!(!result.output.contains('*'));
    }

    #[test]
    fn markdown_to_html_rejects_amplified_output_during_rendering() {
        // review 十四轮修复回归：深度嵌套 blockquote 把百 KB 级入参
        // 放大到 >8 MiB HTML，必须在渲染过程中经 LimitedBuffer 拒绝，
        // 而非先分配完整结果再事后检查。
        let markdown = ">".repeat(400_000);
        assert!(markdown.len() <= COMPUTE_INPUT_MAX_BYTES);
        let error = render_markdown_html(&markdown).unwrap_err();
        assert!(
            error.to_string().contains("output limit"),
            "unexpected error: {error}"
        );
        // 合法输入不受影响
        assert_eq!(render_markdown_html("# Title").unwrap(), "<h1>Title</h1>");
    }

    #[tokio::test]
    async fn date_tool_actions() {
        let result = run(
            &DateTool,
            serde_json::json!({"action": "from_epoch_ms", "epoch_ms": 0}),
        )
        .await;
        assert_eq!(result.output, "1970-01-01T00:00:00Z");
        for (epoch_ms, expected) in [
            (ISO_UTC_MIN_EPOCH_MS, "0000-01-01T00:00:00Z"),
            (ISO_UTC_MAX_EPOCH_MS, "9999-12-31T23:59:59.999Z"),
        ] {
            let result = run(
                &DateTool,
                serde_json::json!({"action": "from_epoch_ms", "epoch_ms": epoch_ms}),
            )
            .await;
            assert_eq!(result.output, expected);
        }
        for epoch_ms in [1, 123, 999, -1, -999] {
            let iso = run(
                &DateTool,
                serde_json::json!({"action": "from_epoch_ms", "epoch_ms": epoch_ms}),
            )
            .await;
            assert!(!iso.is_error, "{}", iso.output);
            let roundtrip = run(
                &DateTool,
                serde_json::json!({"action": "to_epoch_ms", "iso": iso.output}),
            )
            .await;
            assert_eq!(roundtrip.output, epoch_ms.to_string());
        }
        for epoch_ms in [ISO_UTC_MIN_EPOCH_MS - 1, ISO_UTC_MAX_EPOCH_MS + 1] {
            let mut metadata = ToolMetadata::new();
            let mut ctx = ToolContext {
                cwd: Path::new("/tmp"),
                metadata: &mut metadata,
            };
            let result = DateTool
                .execute(
                    serde_json::json!({"action": "from_epoch_ms", "epoch_ms": epoch_ms}),
                    &mut ctx,
                )
                .await;
            assert!(matches!(result, Err(ToolError::InvalidInput(_))));
        }
        let result = run(
            &DateTool,
            serde_json::json!({"action": "to_epoch_ms", "iso": "1970-01-02T00:00:00Z"}),
        )
        .await;
        assert_eq!(result.output, "86400000");
        let result = run(
            &DateTool,
            serde_json::json!({
                "action": "diff_days",
                "from_iso": "2026-07-01T00:00:00Z",
                "to_iso": "2026-07-29T12:00:00Z"
            }),
        )
        .await;
        assert_eq!(result.output, "28");
        // 负区间向负无穷取整（review 二轮：同 Python // 口径）
        let result = run(
            &DateTool,
            serde_json::json!({
                "action": "diff_days",
                "from_iso": "2026-07-02T12:00:00Z",
                "to_iso": "2026-07-02T00:00:00Z"
            }),
        )
        .await;
        assert_eq!(result.output, "-1");
        let result = run(&DateTool, serde_json::json!({"action": "now"})).await;
        assert!(result.output.contains("epoch_ms:"));

        for invalid in [
            "2021-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "10000-01-01T00:00:00Z",
        ] {
            let mut metadata = ToolMetadata::new();
            let mut ctx = ToolContext {
                cwd: Path::new("/tmp"),
                metadata: &mut metadata,
            };
            let result = DateTool
                .execute(
                    serde_json::json!({"action": "to_epoch_ms", "iso": invalid}),
                    &mut ctx,
                )
                .await;
            assert!(
                matches!(result, Err(ToolError::InvalidInput(_))),
                "accepted invalid timestamp {invalid}"
            );
        }
    }

    #[test]
    fn register_compute_tools_registers_all_five() {
        let mut runtime = crate::tools::ToolRuntime::new();
        register_compute_tools(&mut runtime);
        assert_eq!(runtime.len(), 5);
        for name in ["calculator", "json", "text", "markdown", "date"] {
            assert!(runtime.get(name).is_some(), "missing {name}");
            assert!(runtime.get(name).unwrap().is_read_only(&Value::Null));
        }
    }
}
