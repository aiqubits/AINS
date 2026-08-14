//! MCP Client Runtime + McpTool 桥接（对齐 Harness `mcp/` + `mcp_tool.py`）。
//!
//! - 传输：stdio（仅 Native，子进程 + newline-delimited JSON-RPC）/
//!   streamable-http（双端，POST JSON-RPC，响应支持 application/json 与
//!   text/event-stream 两种载体；`Mcp-Session-Id` 会话头透传）。
//! - MCP 工具桥接为普通 `Tool`（名字 `mcp__{server}__{tool}`）注入同一注册表；
//!   Kernel 不感知工具来源。
//! - 单 server 连接失败只记录 failed 状态，不阻断启动（对齐基线）。
//! - ws 传输不支持（对齐基线 "Unsupported MCP transport in current build"）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::ToolError;
use crate::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const MCP_HTTP_TIMEOUT_SECS: u64 = 30;
const MCP_HTTP_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MCP_MAX_TOOL_LIST_PAGES: usize = 100;
const MCP_MAX_DISCOVERED_TOOLS: usize = 1_024;
const MCP_MAX_RETAINED_LIST_BYTES: usize = 8 * 1024 * 1024;
#[cfg(not(target_arch = "wasm32"))]
const MCP_STDIO_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
#[cfg(not(target_arch = "wasm32"))]
const MCP_STDIO_TIMEOUT_SECS: u64 = 30;

// ── 配置与状态类型（对齐 mcp/types.py）─────────────────────────────────

/// MCP server 配置（`type` 判别式：stdio / http；ws 保留变体仅报不支持）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
        #[serde(default)]
        cwd: Option<String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    Ws {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

impl McpServerConfig {
    fn transport_name(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
            Self::Ws { .. } => "ws",
        }
    }
}

/// MCP server 暴露的工具元数据（对齐 `McpToolInfo`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub server_name: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpConnectionState {
    Connected,
    Failed,
    Pending,
    Disabled,
}

/// 单 server 运行状态（对齐 `McpConnectionStatus`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConnectionStatus {
    pub name: String,
    pub state: McpConnectionState,
    #[serde(default)]
    pub detail: String,
    pub transport: String,
    #[serde(default)]
    pub auth_configured: bool,
    #[serde(default)]
    pub tools: Vec<McpToolInfo>,
}

// ── JSON-RPC 传输 ───────────────────────────────────────────────────────

/// 单 server 的 JSON-RPC 客户端传输（内部可变，经 manager 的异步锁串行化）。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
trait McpTransport: crate::marker::MaybeSendSync {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String>;
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String>;
}

/// JSON-RPC 响应解包：`result` 或 `error.message`。
fn unpack_jsonrpc_response(message: &Value) -> Result<Value, String> {
    if let Some(error) = message.get("error") {
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown JSON-RPC error");
        return Err(format!("JSON-RPC error: {text}"));
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

fn unpack_jsonrpc_response_for_id(message: &Value, expect_id: u64) -> Result<Value, String> {
    if message.get("id").and_then(Value::as_u64) != Some(expect_id) {
        return Err(format!(
            "MCP HTTP response id did not match request id {expect_id}"
        ));
    }
    unpack_jsonrpc_response(message)
}

/// Incrementally consume SSE and return as soon as the matching JSON-RPC
/// response arrives. Streamable HTTP responses are commonly long-lived, so
/// waiting for EOF would turn a successful response into a timeout.
async fn extract_sse_response_stream<S, B, E>(stream: S, expect_id: u64) -> Result<Value, String>
where
    S: futures::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    futures::pin_mut!(stream);
    let mut pending = Vec::new();
    let mut event_data = Vec::new();
    let mut received = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        let chunk = chunk.as_ref();
        received = received
            .checked_add(chunk.len())
            .ok_or("MCP SSE response size overflow")?;
        if received > MCP_HTTP_MAX_RESPONSE_BYTES {
            return Err(format!(
                "MCP HTTP response exceeded {} bytes",
                MCP_HTTP_MAX_RESPONSE_BYTES
            ));
        }
        pending.extend_from_slice(chunk);
        while let Some(line) = take_sse_line(&mut pending, false) {
            if append_sse_line(&line, &mut event_data)
                && let Some(result) = parse_sse_event(&mut event_data, expect_id)?
            {
                return Ok(result);
            }
        }
    }
    while let Some(line) = take_sse_line(&mut pending, true) {
        if append_sse_line(&line, &mut event_data)
            && let Some(result) = parse_sse_event(&mut event_data, expect_id)?
        {
            return Ok(result);
        }
    }
    if !pending.is_empty() {
        let line = std::mem::take(&mut pending);
        append_sse_line(&line, &mut event_data);
    }
    if let Some(result) = parse_sse_event(&mut event_data, expect_id)? {
        return Ok(result);
    }
    Err("SSE stream ended without a matching JSON-RPC response".into())
}

/// Remove one SSE line, accepting LF, CRLF, and CR delimiters. A trailing CR
/// is held until another chunk arrives so a split CRLF pair is consumed once.
fn take_sse_line(pending: &mut Vec<u8>, eof: bool) -> Option<Vec<u8>> {
    let index = pending
        .iter()
        .position(|byte| matches!(*byte, b'\r' | b'\n'))?;
    if pending[index] == b'\r' && index + 1 == pending.len() && !eof {
        return None;
    }
    let delimiter_len =
        usize::from(pending[index] == b'\r' && pending.get(index + 1).copied() == Some(b'\n')) + 1;
    let line = pending.drain(..index).collect();
    pending.drain(..delimiter_len);
    Some(line)
}

/// Accumulate the `data` fields for one SSE event. Per the SSE framing rules,
/// multiple fields are joined with LF and dispatched only on a blank line.
fn append_sse_line(line: &[u8], event_data: &mut Vec<u8>) -> bool {
    if line.is_empty() {
        return true;
    }
    if line.first() == Some(&b':') {
        return false;
    }
    let (field, mut value) = match line.iter().position(|byte| *byte == b':') {
        Some(index) => (&line[..index], &line[index + 1..]),
        None => (line, &[][..]),
    };
    if field == b"data" {
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        event_data.extend_from_slice(value);
        event_data.push(b'\n');
    }
    false
}

fn parse_sse_event(event_data: &mut Vec<u8>, expect_id: u64) -> Result<Option<Value>, String> {
    if event_data.is_empty() {
        return Ok(None);
    }
    if event_data.last() == Some(&b'\n') {
        event_data.pop();
    }
    let data = std::str::from_utf8(event_data).map_err(|error| error.to_string())?;
    let parsed = serde_json::from_str::<Value>(data.trim());
    event_data.clear();
    let Ok(message) = parsed else {
        return Ok(None);
    };
    if message.get("id").and_then(Value::as_u64) == Some(expect_id) {
        unpack_jsonrpc_response(&message).map(Some)
    } else {
        Ok(None)
    }
}

/// streamable-http 传输（双端）：POST JSON-RPC 到固定 URL。
///
/// 信任边界：server URL 仅来自宿主/用户配置（可信输入，允许指向
/// 内网/本机 MCP 服务），因此此处不做 `web_fetch` 式的 SSRF 公网
/// 校验。若未来允许模型输出或远程 profile 动态添加 MCP server，
/// 必须先接入 `network::ensure_public_http_url` 同口径的目标校验，
/// 否则会静默继承 SSRF 向量。
struct HttpTransport {
    url: String,
    headers: HashMap<String, String>,
    client: reqwest::Client,
    session_id: Option<String>,
    next_id: u64,
    request_timeout: std::time::Duration,
}

impl HttpTransport {
    fn new(url: String, headers: HashMap<String, String>) -> Result<Self, String> {
        Self::new_with_timeout(
            url,
            headers,
            std::time::Duration::from_secs(MCP_HTTP_TIMEOUT_SECS),
        )
    }

    fn new_with_timeout(
        url: String,
        headers: HashMap<String, String>,
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        #[cfg(not(target_arch = "wasm32"))]
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| error.to_string())?;
        #[cfg(target_arch = "wasm32")]
        let client = reqwest::Client::new();

        Ok(Self {
            url,
            headers,
            client,
            session_id: None,
            next_id: 1,
            request_timeout: timeout,
        })
    }

    async fn post(&mut self, payload: Value, expect_id: Option<u64>) -> Result<Value, String> {
        let mut request = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        if let Some(session_id) = &self.session_id {
            request = request.header("mcp-session-id", session_id.as_str());
        }
        let response = request
            .json(&payload)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            // 失败响应（如代理 401）不得改写既有会话 id
            //（review 十二轮修复）：会话头仅从成功响应采纳。
            // 错误消息附带响应来源（remote addr），便于诊断代理/中间层干扰。
            // WASM 的 fetch 后端无 TCP peer 概念，恒为 unknown。
            #[cfg(not(target_arch = "wasm32"))]
            let remote = response
                .remote_addr()
                .map(|addr| addr.to_string())
                .unwrap_or_else(|| "unknown".into());
            #[cfg(target_arch = "wasm32")]
            let remote = "unknown";
            return Err(format!("MCP server returned HTTP {status} (from {remote})"));
        }
        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session_id.to_string());
        }
        let Some(expect_id) = expect_id else {
            return Ok(Value::Null); // notification：无响应体要求
        };
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if content_type.contains("text/event-stream") {
            extract_sse_response_stream(response.bytes_stream(), expect_id).await
        } else {
            let body = crate::tools::network::read_response_text_limited(
                response,
                MCP_HTTP_MAX_RESPONSE_BYTES,
            )
            .await?;
            let message: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
            unpack_jsonrpc_response_for_id(&message, expect_id)
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl McpTransport for HttpTransport {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        run_with_timeout(self.request_timeout, self.post(payload, Some(id))).await
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let payload = json!({"jsonrpc": "2.0", "method": method, "params": params});
        run_with_timeout(self.request_timeout, self.post(payload, None))
            .await
            .map(|_| ())
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn run_with_timeout<F, T>(duration: std::time::Duration, future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    match tokio::time::timeout(duration, future).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "MCP HTTP request timed out after {}s",
            duration.as_secs_f64()
        )),
    }
}

#[cfg(target_arch = "wasm32")]
async fn run_with_timeout<F, T>(duration: std::time::Duration, future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    use crate::runtime_adapter::RuntimeAdapter;
    use futures::future::{Either, select};

    match select(
        Box::pin(future),
        Box::pin(crate::WasmRuntimeAdapter::sleep(duration)),
    )
    .await
    {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(format!(
            "MCP HTTP request timed out after {}s",
            duration.as_secs_f64()
        )),
    }
}

/// stdio 传输（仅 Native）：子进程 stdin/stdout 上的 newline-delimited JSON-RPC。
#[cfg(not(target_arch = "wasm32"))]
struct StdioTransport {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl StdioTransport {
    fn spawn(
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
        cwd: Option<&str>,
    ) -> Result<Self, String> {
        use std::process::Stdio;
        let mut builder = tokio::process::Command::new(command);
        builder
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(env) = env {
            builder.envs(env);
        }
        if let Some(cwd) = cwd {
            builder.current_dir(cwd);
        }
        let mut child = builder.spawn().map_err(|error| error.to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or("failed to open MCP server stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("failed to open MCP server stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
            next_id: 1,
        })
    }

    async fn send_line(&mut self, payload: &Value) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let mut line = payload.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        self.stdin.flush().await.map_err(|error| error.to_string())
    }

    /// 写入侧同样有界（review 十二轮修复）：子进程停止读 stdin 时
    /// 管道写满会让 send 无限挂起，与读侧同口径 30s 超时。
    async fn send_line_timed(&mut self, payload: &Value) -> Result<(), String> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(MCP_STDIO_TIMEOUT_SECS),
            self.send_line(payload),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(format!(
                "MCP stdio write timed out after {MCP_STDIO_TIMEOUT_SECS}s"
            )),
        }
    }

    /// 读取直到出现 id 匹配的响应；其间的通知/无关消息跳过。
    async fn read_response(&mut self, expect_id: u64) -> Result<Value, String> {
        // 单请求最多容忍 256 行无关消息，防协议违规死循环
        for _ in 0..256 {
            let Some(line) = read_line_bounded(&mut self.stdout, MCP_STDIO_MAX_FRAME_BYTES).await?
            else {
                return Err("MCP server closed stdout".into());
            };
            let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) == Some(expect_id) {
                return unpack_jsonrpc_response(&message);
            }
        }
        Err("MCP server flooded stdout without answering the request".into())
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn read_line_bounded<R>(reader: &mut R, max_bytes: usize) -> Result<Option<Vec<u8>>, String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|error| error.to_string())?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > max_bytes {
            return Err(format!("MCP stdio frame exceeded {max_bytes} bytes"));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl McpTransport for StdioTransport {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_line_timed(&payload).await?;
        // 30s 请求超时（初始化/工具调用同口径；防挂死）
        match tokio::time::timeout(
            std::time::Duration::from_secs(MCP_STDIO_TIMEOUT_SECS),
            self.read_response(id),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(format!(
                "MCP stdio request timed out after {MCP_STDIO_TIMEOUT_SECS}s"
            )),
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let payload = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_line_timed(&payload).await
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for StdioTransport {
    fn drop(&mut self) {
        // kill_on_drop 已兜底；此处显式尝试立即回收
        let _ = self.child.start_kill();
    }
}

// ── Client Manager ──────────────────────────────────────────────────────

type SharedTransport = Arc<futures::lock::Mutex<Box<dyn McpTransport>>>;

/// MCP 连接管理器（对齐 `McpClientManager`）。
#[derive(Default)]
pub struct McpClientManager {
    configs: Vec<(String, McpServerConfig)>,
    statuses: HashMap<String, McpConnectionStatus>,
    sessions: HashMap<String, SharedTransport>,
}

impl McpClientManager {
    pub fn new(configs: Vec<(String, McpServerConfig)>) -> Self {
        let statuses = configs
            .iter()
            .map(|(name, config)| {
                (
                    name.clone(),
                    McpConnectionStatus {
                        name: name.clone(),
                        state: McpConnectionState::Pending,
                        detail: String::new(),
                        transport: config.transport_name().to_string(),
                        auth_configured: false,
                        tools: Vec::new(),
                    },
                )
            })
            .collect();
        Self {
            configs,
            statuses,
            sessions: HashMap::new(),
        }
    }

    /// 连接全部配置的 server：单 server 失败仅记录状态，不阻断启动。
    pub async fn connect_all(&mut self) {
        let configs = self.configs.clone();
        for (name, config) in configs {
            // 重连从干净状态开始；握手完全成功前不得继续暴露旧 transport。
            self.sessions.remove(&name);
            match self.connect_one(&name, &config).await {
                Ok(()) => {}
                Err(detail) => {
                    self.mark_failed(&name, &config, detail);
                }
            }
        }
    }

    // WASM 单线程宿主：dyn McpTransport 不满足 Send+Sync 属预期（MaybeSendSync
    // 在 wasm 上为空约束），Arc 仅用于引用计数不跨线程。
    #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
    async fn connect_one(&mut self, name: &str, config: &McpServerConfig) -> Result<(), String> {
        let auth_configured = match config {
            McpServerConfig::Stdio { env, .. } => env.as_ref().is_some_and(|env| !env.is_empty()),
            McpServerConfig::Http { headers, .. } | McpServerConfig::Ws { headers, .. } => {
                !headers.is_empty()
            }
        };
        let transport: Box<dyn McpTransport> = match config {
            McpServerConfig::Http { url, headers } => {
                Box::new(HttpTransport::new(url.clone(), headers.clone())?)
            }
            #[cfg(not(target_arch = "wasm32"))]
            McpServerConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => Box::new(StdioTransport::spawn(
                command,
                args,
                env.as_ref(),
                cwd.as_deref(),
            )?),
            #[cfg(target_arch = "wasm32")]
            McpServerConfig::Stdio { .. } => {
                return Err("Unsupported MCP transport in current build: stdio".into());
            }
            McpServerConfig::Ws { .. } => {
                return Err("Unsupported MCP transport in current build: ws".into());
            }
        };
        let shared: SharedTransport = Arc::new(futures::lock::Mutex::new(transport));

        // initialize → notifications/initialized → tools/list（对齐 MCP 握手）
        let tools = {
            let mut guard = shared.lock().await;
            let initialized = guard
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {"name": "ains-rust-agent", "version": env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await?;
            validate_protocol_version(&initialized)?;
            guard.notify("notifications/initialized", json!({})).await?;
            list_all_tools(name, guard.as_mut()).await?
        };

        self.sessions.insert(name.to_string(), shared);
        self.statuses.insert(
            name.to_string(),
            McpConnectionStatus {
                name: name.to_string(),
                state: McpConnectionState::Connected,
                detail: String::new(),
                transport: config.transport_name().to_string(),
                auth_configured,
                tools,
            },
        );
        Ok(())
    }

    fn mark_failed(&mut self, name: &str, config: &McpServerConfig, detail: String) {
        self.sessions.remove(name);
        let auth_configured = match config {
            McpServerConfig::Stdio { env, .. } => env.as_ref().is_some_and(|env| !env.is_empty()),
            McpServerConfig::Http { headers, .. } | McpServerConfig::Ws { headers, .. } => {
                !headers.is_empty()
            }
        };
        self.statuses.insert(
            name.to_string(),
            McpConnectionStatus {
                name: name.to_string(),
                state: McpConnectionState::Failed,
                detail,
                transport: config.transport_name().to_string(),
                auth_configured,
                tools: Vec::new(),
            },
        );
    }

    /// 全部 server 状态（按名字排序，对齐基线 list_statuses）。
    pub fn list_statuses(&self) -> Vec<&McpConnectionStatus> {
        let mut names: Vec<&String> = self.statuses.keys().collect();
        names.sort();
        names.into_iter().map(|name| &self.statuses[name]).collect()
    }

    /// 全部已连接 server 的工具（桥接注册用）。
    pub fn list_tools(&self) -> Vec<McpToolInfo> {
        self.list_statuses()
            .into_iter()
            .flat_map(|status| status.tools.iter().cloned())
            .collect()
    }

    /// 取单 server 的会话句柄（克隆 Arc，供调用方在不持 manager 锁的
    /// 情况下发起 RPC，避免把全部 MCP 调用串行化）。
    fn session_handle(&self, server_name: &str) -> Result<SharedTransport, String> {
        let status = self.statuses.get(server_name);
        if !status.is_some_and(|status| status.state == McpConnectionState::Connected) {
            let detail = status
                .map(|status| status.detail.clone())
                .unwrap_or_else(|| "unknown server".into());
            return Err(format!(
                "MCP server '{server_name}' is not connected: {detail}"
            ));
        }
        match self.sessions.get(server_name) {
            Some(session) => Ok(session.clone()),
            None => {
                let detail = self
                    .statuses
                    .get(server_name)
                    .map(|status| status.detail.clone())
                    .unwrap_or_else(|| "unknown server".into());
                Err(format!(
                    "MCP server '{server_name}' is not connected: {detail}"
                ))
            }
        }
    }

    /// 调用一个 MCP 工具，保留协议级 `isError` 语义。
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpCallOutput, String> {
        let session = self.session_handle(server_name)?;
        call_tool_on(session, server_name, tool_name, arguments).await
    }
}

/// 在已取得的会话句柄上执行 tools/call（仅持单 server 的 transport 锁，
/// 不阻塞其它 server 的并发调用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutput {
    pub output: String,
    pub is_error: bool,
}

async fn call_tool_on(
    session: SharedTransport,
    server_name: &str,
    tool_name: &str,
    arguments: Value,
) -> Result<McpCallOutput, String> {
    let result = {
        let mut guard = session.lock().await;
        guard
            .request(
                "tools/call",
                json!({"name": tool_name, "arguments": arguments}),
            )
            .await
            .map_err(|error| format!("MCP server '{server_name}' call failed: {error}"))?
    };
    Ok(McpCallOutput {
        output: stringify_call_result(&result),
        is_error: result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_tool_list(server_name: &str, listed: &Value) -> Vec<McpToolInfo> {
    listed
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name")?.as_str()?.to_string();
                    let description = tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let input_schema = tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    Some(McpToolInfo {
                        server_name: server_name.to_string(),
                        name,
                        description,
                        input_schema,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn validate_protocol_version(initialized: &Value) -> Result<(), String> {
    let negotiated = initialized
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or("MCP initialize response did not include a string protocolVersion")?;
    if negotiated != MCP_PROTOCOL_VERSION {
        return Err(format!(
            "MCP server selected unsupported protocol version '{negotiated}' (supported: {MCP_PROTOCOL_VERSION})"
        ));
    }
    Ok(())
}

async fn list_all_tools(
    server_name: &str,
    transport: &mut dyn McpTransport,
) -> Result<Vec<McpToolInfo>, String> {
    let mut tools = Vec::new();
    let mut retained_bytes = 0_usize;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    for _ in 0..MCP_MAX_TOOL_LIST_PAGES {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
        // The request payload owns its cursor value, so the previous cursor can
        // move into the seen set without keeping a second persistent copy.
        if let Some(previous_cursor) = cursor.take() {
            let inserted = seen_cursors.insert(previous_cursor);
            debug_assert!(inserted);
        }
        let listed = transport.request("tools/list", params).await?;
        let page_tools = parse_tool_list(server_name, &listed);
        let discovered_count = tools
            .len()
            .checked_add(page_tools.len())
            .ok_or_else(|| format!("MCP server '{server_name}' tools/list count overflow"))?;
        if discovered_count > MCP_MAX_DISCOVERED_TOOLS {
            return Err(format!(
                "MCP server '{server_name}' exceeded the {MCP_MAX_DISCOVERED_TOOLS}-tool discovery limit"
            ));
        }

        let page_bytes = page_tools.iter().try_fold(0_usize, |total, tool| {
            let serialized = serde_json::to_vec(tool).map_err(|error| {
                format!("MCP server '{server_name}' returned invalid tool metadata: {error}")
            })?;
            total
                .checked_add(serialized.len())
                .ok_or_else(|| format!("MCP server '{server_name}' tools/list size overflow"))
        })?;
        let discovered_bytes = retained_bytes
            .checked_add(page_bytes)
            .ok_or_else(|| format!("MCP server '{server_name}' tools/list size overflow"))?;
        if discovered_bytes > MCP_MAX_RETAINED_LIST_BYTES {
            return Err(format!(
                "MCP server '{server_name}' exceeded the {MCP_MAX_RETAINED_LIST_BYTES}-byte retained tools/list state limit"
            ));
        }

        tools.extend(page_tools);
        retained_bytes = discovered_bytes;

        let Some(next_cursor) = listed.get("nextCursor") else {
            return Ok(tools);
        };
        if next_cursor.is_null() {
            return Ok(tools);
        }
        let next_cursor = next_cursor
            .as_str()
            .ok_or("MCP tools/list nextCursor must be a string when present")?;
        if seen_cursors.contains(next_cursor) {
            return Err(format!(
                "MCP server '{server_name}' repeated tools/list cursor; aborting pagination"
            ));
        }
        let discovered_bytes = retained_bytes
            .checked_add(next_cursor.len())
            .ok_or_else(|| format!("MCP server '{server_name}' tools/list size overflow"))?;
        if discovered_bytes > MCP_MAX_RETAINED_LIST_BYTES {
            return Err(format!(
                "MCP server '{server_name}' exceeded the {MCP_MAX_RETAINED_LIST_BYTES}-byte retained tools/list state limit"
            ));
        }
        retained_bytes = discovered_bytes;
        cursor = Some(next_cursor.to_string());
    }
    Err(format!(
        "MCP server '{server_name}' exceeded the {MCP_MAX_TOOL_LIST_PAGES}-page tools/list limit"
    ))
}

/// CallToolResult 字符串化（对齐基线）：text 项取文本，其余项 JSON dump；
/// 空则回落 structuredContent；再空回落 "(no output)"。
fn stringify_call_result(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    parts.push(text.to_string());
                }
            } else {
                parts.push(item.to_string());
            }
        }
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
        && !structured.is_null()
    {
        parts.push(structured.to_string());
    }
    if parts.is_empty() {
        parts.push("(no output)".to_string());
    }
    parts.join("\n").trim().to_string()
}

// ── McpTool 桥接（对齐 mcp_tool.py::McpToolAdapter）────────────────────

/// 工具名段安全化（对齐 `_sanitize_tool_segment`）。
fn sanitize_tool_segment(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        return "tool".to_string();
    }
    if !sanitized
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        return format!("mcp_{sanitized}");
    }
    sanitized
}

/// 把一个 MCP 工具暴露为普通 AINS Tool。
pub struct McpToolAdapter {
    manager: Arc<futures::lock::Mutex<McpClientManager>>,
    info: McpToolInfo,
    bridged_name: String,
}

impl McpToolAdapter {
    pub fn new(manager: Arc<futures::lock::Mutex<McpClientManager>>, info: McpToolInfo) -> Self {
        let bridged_name = format!(
            "mcp__{}__{}",
            sanitize_tool_segment(&info.server_name),
            sanitize_tool_segment(&info.name)
        );
        Self {
            manager,
            info,
            bridged_name,
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Tool for McpToolAdapter {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: self.bridged_name.clone(),
            description: if self.info.description.is_empty() {
                format!("MCP tool {}", self.info.name)
            } else {
                self.info.description.clone()
            },
            input_schema: self.info.input_schema.clone(),
        }
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // null 输入归一为空对象（远端 schema 校验自理）
        let arguments = match input {
            Value::Null => Value::Object(Map::new()),
            other => other,
        };
        // 短暂持 manager 锁仅取会话句柄；RPC 等待期间只持单 server 的
        // transport 锁，不同 server 的 MCP 调用可并发（review 修复：旧实现
        // 持 manager 锁跨越整个网络 I/O，所有 MCP 调用被串行化）。
        let session = {
            let manager = self.manager.lock().await;
            manager.session_handle(&self.info.server_name)
        };
        let session = match session {
            Ok(session) => session,
            Err(error) => return Ok(ToolResult::err(error)),
        };
        match call_tool_on(session, &self.info.server_name, &self.info.name, arguments).await {
            Ok(result) => Ok(ToolResult {
                output: result.output,
                is_error: result.is_error,
                metadata: Value::Null,
            }),
            Err(error) => Ok(ToolResult::err(error)),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Network
    }
}

/// 把全部已连接 MCP 工具桥接进注册表（宿主启动时调用）。
pub async fn register_mcp_tools(
    runtime: &mut crate::tools::ToolRuntime,
    manager: Arc<futures::lock::Mutex<McpClientManager>>,
) -> Result<(), String> {
    let tools = {
        let guard = manager.lock().await;
        guard.list_tools()
    };
    let mut adapters = Vec::with_capacity(tools.len());
    let mut origins = HashMap::<String, String>::new();
    for info in tools {
        let origin = format!("{}::{}", info.server_name, info.name);
        let adapter = McpToolAdapter::new(manager.clone(), info);
        let bridged_name = adapter.definition().name;
        if runtime.get(&bridged_name).is_some() {
            return Err(format!(
                "MCP tool name collision: '{origin}' maps to existing tool '{bridged_name}'"
            ));
        }
        if let Some(previous) = origins.insert(bridged_name.clone(), origin.clone()) {
            return Err(format!(
                "MCP tool name collision: '{previous}' and '{origin}' both map to '{bridged_name}'"
            ));
        }
        adapters.push(adapter);
    }
    for adapter in adapters {
        runtime.register(Box::new(adapter));
    }
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn sanitize_segments_align_baseline() {
        assert_eq!(sanitize_tool_segment("files"), "files");
        assert_eq!(sanitize_tool_segment("my server!"), "my_server_");
        assert_eq!(sanitize_tool_segment(""), "tool");
        assert_eq!(sanitize_tool_segment("1st"), "mcp_1st");
        assert_eq!(sanitize_tool_segment("_x"), "mcp__x");
    }

    #[test]
    fn call_result_stringification() {
        let result = json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "data": "…"}
        ]});
        let text = stringify_call_result(&result);
        assert!(text.starts_with("hello\n"));
        assert!(text.contains(r#""type":"image""#));
        // 空 content 回落 structuredContent
        let result = json!({"content": [], "structuredContent": {"a": 1}});
        assert_eq!(stringify_call_result(&result), r#"{"a":1}"#);
        // 空、空白或缺失 text 不应阻断 structuredContent 回落。
        for content in [
            json!([{"type": "text", "text": ""}]),
            json!([{"type": "text", "text": " \n\t"}]),
            json!([{"type": "text"}]),
        ] {
            let result = json!({"content": content, "structuredContent": {"a": 1}});
            assert_eq!(stringify_call_result(&result), r#"{"a":1}"#);
        }
        assert_eq!(
            stringify_call_result(&json!({"content": [{"type": "text", "text": ""}]})),
            "(no output)"
        );
        // 全空
        assert_eq!(stringify_call_result(&json!({})), "(no output)");
    }

    #[tokio::test]
    async fn call_tool_preserves_mcp_is_error() {
        struct ErrorTransport;

        #[async_trait::async_trait]
        impl McpTransport for ErrorTransport {
            async fn request(&mut self, _method: &str, _params: Value) -> Result<Value, String> {
                Ok(json!({
                    "content": [{"type": "text", "text": "remote failure"}],
                    "isError": true
                }))
            }

            async fn notify(&mut self, _method: &str, _params: Value) -> Result<(), String> {
                Ok(())
            }
        }

        let session: SharedTransport =
            Arc::new(futures::lock::Mutex::new(Box::new(ErrorTransport)));
        let result = call_tool_on(session, "srv", "broken", json!({}))
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(result.output, "remote failure");
    }

    #[tokio::test]
    async fn http_timeout_wrapper_rejects_stalled_request() {
        let stalled = futures::future::pending::<Result<(), String>>();
        let error = run_with_timeout(std::time::Duration::from_millis(10), stalled)
            .await
            .unwrap_err();
        assert!(error.to_lowercase().contains("timed out"), "{error}");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn http_error_response_does_not_adopt_session_id() {
        // review 十二轮修复回归：失败响应（如代理 401）携带的
        // mcp-session-id 不得覆盖/建立会话；成功响应正常采纳。
        //
        // 本测试曾因宿主环境注入的 HTTP 代理变量（如 `http_proxy` 指向本机
        // 代理端口）间歇性失败：reqwest 把针对测试服务器的请求转发给代理，
        // 代理无法代理到本地 ephemeral 端口并返回 400 Bad Request（测试
        // 服务器从未收到连接，客户端却收到 400）。修复：测试 client 显式
        // `no_proxy()`（本测试验证的是错误响应不采纳 session id 的语义，
        // 与代理无关），并禁用连接复用（`pool_max_idle_per_host(0)`，请求
        // 与连接严格一一对应）；服务器读取完整请求头（容忍分片）后再响应。
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let success_body = r#"{"jsonrpc":"2.0","id":2,"result":{"ok":true}}"#;
        let responses = [
            "HTTP/1.1 401 Unauthorized\r\nmcp-session-id: hijacked\r\n\
             content-length: 0\r\nconnection: close\r\n\r\n"
                .to_string(),
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 mcp-session-id: real\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{success_body}",
                success_body.len()
            ),
        ];

        // 测试专用 client：禁用代理（宿主环境的 http_proxy 会把本地测试请求
        // 劫持到代理端口返回 400）与连接复用（请求/连接一一对应）。
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(MCP_HTTP_TIMEOUT_SECS))
            .no_proxy()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap();
        // 服务器观察日志（内存，无 I/O）：诊断失败时输出服务器视角。
        let server_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let server_log_clone = std::sync::Arc::clone(&server_log);
        let server = tokio::spawn(async move {
            for (i, response) in responses.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0u8; 4096];
                let mut received = 0usize;
                loop {
                    match socket.read(&mut buffer[received..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            received += n;
                            if received == buffer.len()
                                || buffer[..received].windows(4).any(|w| w == b"\r\n\r\n")
                            {
                                break;
                            }
                        }
                    }
                }
                server_log_clone.lock().unwrap().push(format!(
                    "conn {i}: read {received} bytes: {:?}",
                    String::from_utf8_lossy(&buffer[..received.min(60)])
                ));
                if socket.write_all(response.as_bytes()).await.is_err() {
                    server_log_clone
                        .lock()
                        .unwrap()
                        .push(format!("conn {i}: write failed"));
                    break;
                }
                server_log_clone.lock().unwrap().push(format!(
                    "conn {i}: wrote {:?}",
                    &response[..40.min(response.len())]
                ));
                let _ = socket.shutdown().await;
            }
        });
        let mut transport = HttpTransport {
            url: format!("http://{address}/mcp"),
            headers: HashMap::new(),
            client,
            session_id: None,
            next_id: 1,
            request_timeout: std::time::Duration::from_secs(MCP_HTTP_TIMEOUT_SECS),
        };
        let error = transport
            .request("tools/list", json!({}))
            .await
            .unwrap_err();
        if !error.contains("HTTP 401") {
            let log = server_log
                .lock()
                .map(|log| log.join("; "))
                .unwrap_or_else(|_| "(log poisoned)".into());
            panic!(
                "expected HTTP 401 error, got: {error} | url: {} | server log: {log}",
                transport.url
            );
        }
        assert_eq!(
            transport.session_id, None,
            "error response must not establish a session id"
        );

        let result = transport.request("tools/list", json!({})).await.unwrap();
        assert_eq!(result, json!({"ok": true}));
        assert_eq!(transport.session_id.as_deref(), Some("real"));
        server.await.unwrap();
    }

    #[test]
    fn tool_list_parsing_defaults_schema() {
        let listed = json!({"tools": [
            {"name": "a", "description": "d", "inputSchema": {"type": "object"}},
            {"name": "b"},
            {"missing_name": true}
        ]});
        let tools = parse_tool_list("srv", &listed);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "a");
        assert_eq!(
            tools[1].input_schema,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(tools[1].server_name, "srv");
    }

    struct PaginatedTransport {
        responses: std::collections::VecDeque<Value>,
        params: Vec<Value>,
    }

    #[async_trait::async_trait]
    impl McpTransport for PaginatedTransport {
        async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
            assert_eq!(method, "tools/list");
            self.params.push(params);
            self.responses
                .pop_front()
                .ok_or_else(|| "unexpected tools/list request".to_string())
        }

        async fn notify(&mut self, _method: &str, _params: Value) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn tool_list_follows_all_pages_and_preserves_empty_cursor() {
        let mut transport = PaginatedTransport {
            responses: [
                json!({"tools": [{"name": "first"}], "nextCursor": ""}),
                json!({"tools": [{"name": "second"}], "nextCursor": "page-3"}),
                json!({"tools": [{"name": "third"}]}),
            ]
            .into(),
            params: Vec::new(),
        };
        let tools = list_all_tools("srv", &mut transport).await.unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(
            transport.params,
            [
                json!({}),
                json!({"cursor": ""}),
                json!({"cursor": "page-3"})
            ]
        );
    }

    #[tokio::test]
    async fn tool_list_rejects_repeated_or_non_string_cursor() {
        let mut repeated = PaginatedTransport {
            responses: [
                json!({"tools": [], "nextCursor": "same"}),
                json!({"tools": [], "nextCursor": "same"}),
            ]
            .into(),
            params: Vec::new(),
        };
        let error = list_all_tools("srv", &mut repeated).await.unwrap_err();
        assert!(error.contains("repeated tools/list cursor"), "{error}");

        let mut invalid = PaginatedTransport {
            responses: [json!({"tools": [], "nextCursor": 42})].into(),
            params: Vec::new(),
        };
        let error = list_all_tools("srv", &mut invalid).await.unwrap_err();
        assert!(error.contains("nextCursor must be a string"), "{error}");
    }

    #[tokio::test]
    async fn tool_list_rejects_unbounded_unique_cursor_chain() {
        let responses = (0..MCP_MAX_TOOL_LIST_PAGES)
            .map(|index| json!({"tools": [], "nextCursor": format!("page-{index}")}))
            .collect();
        let mut transport = PaginatedTransport {
            responses,
            params: Vec::new(),
        };
        let error = list_all_tools("srv", &mut transport).await.unwrap_err();
        assert!(error.contains("tools/list limit"), "{error}");
        assert_eq!(transport.params.len(), MCP_MAX_TOOL_LIST_PAGES);
    }

    #[tokio::test]
    async fn tool_list_rejects_excessive_aggregate_tool_count() {
        let listed_tools = (0..=MCP_MAX_DISCOVERED_TOOLS)
            .map(|index| json!({"name": format!("tool-{index}")}))
            .collect::<Vec<_>>();
        let mut transport = PaginatedTransport {
            responses: [json!({"tools": listed_tools})].into(),
            params: Vec::new(),
        };

        let error = list_all_tools("srv", &mut transport).await.unwrap_err();
        assert!(error.contains("tool discovery limit"), "{error}");
    }

    #[tokio::test]
    async fn tool_list_rejects_excessive_aggregate_retained_bytes() {
        let description = "x".repeat(MCP_MAX_RETAINED_LIST_BYTES / 3);
        let mut transport = PaginatedTransport {
            responses: [
                json!({
                    "tools": [{"name": "first", "description": description}],
                    "nextCursor": "page-2"
                }),
                json!({
                    "tools": [{"name": "second", "description": description}],
                    "nextCursor": "page-3"
                }),
                json!({"tools": [{"name": "third", "description": description}]}),
            ]
            .into(),
            params: Vec::new(),
        };

        let error = list_all_tools("srv", &mut transport).await.unwrap_err();
        assert!(error.contains("retained tools/list state limit"), "{error}");
        assert_eq!(transport.params.len(), 3);
    }

    #[tokio::test]
    async fn tool_list_rejects_excessive_aggregate_cursor_bytes() {
        let cursor_payload = "x".repeat(MCP_MAX_RETAINED_LIST_BYTES / 3 + 1);
        let mut transport = PaginatedTransport {
            responses: ["first", "second", "third"]
                .into_iter()
                .map(|prefix| {
                    json!({
                        "tools": [],
                        "nextCursor": format!("{prefix}-{cursor_payload}")
                    })
                })
                .collect(),
            params: Vec::new(),
        };

        let error = list_all_tools("srv", &mut transport).await.unwrap_err();
        assert!(error.contains("retained tools/list state limit"), "{error}");
        assert_eq!(transport.params.len(), 3);
    }

    #[test]
    fn initialize_protocol_version_must_be_supported() {
        validate_protocol_version(&json!({"protocolVersion": MCP_PROTOCOL_VERSION})).unwrap();
        let error =
            validate_protocol_version(&json!({"protocolVersion": "2099-01-01"})).unwrap_err();
        assert!(error.contains("unsupported protocol version"), "{error}");
        let error = validate_protocol_version(&json!({})).unwrap_err();
        assert!(error.contains("did not include"), "{error}");
    }

    #[tokio::test]
    async fn sse_response_extraction_is_incremental() {
        use futures::stream;

        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        // A pending tail models a persistent SSE connection. Extraction must
        // return after the matching event without waiting for stream EOF.
        let chunks = stream::once(futures::future::ready(Ok::<_, String>(body.as_bytes())))
            .chain(stream::pending());
        let result = extract_sse_response_stream(chunks, 1).await.unwrap();
        assert_eq!(result, json!({"ok": true}));
        // id 不匹配
        let chunks = stream::iter(vec![Ok::<_, String>(body.as_bytes())]);
        assert!(extract_sse_response_stream(chunks, 2).await.is_err());
        // error 载体
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"message\":\"boom\"}}\n";
        let chunks = stream::iter(vec![Ok::<_, String>(body.as_bytes())]);
        let error = extract_sse_response_stream(chunks, 3).await.unwrap_err();
        assert!(error.contains("boom"));
    }

    #[tokio::test]
    async fn sse_response_joins_multiline_data_across_chunks() {
        use futures::stream;

        // JSON whitespace may contain newlines. SSE represents those as multiple
        // data fields, and this fixture also splits both CRLF delimiters.
        let chunks = stream::iter(vec![
            Ok::<_, String>(b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\r".as_slice()),
            Ok::<_, String>(
                b"\ndata: \"id\":7,\r\ndata: \"result\":{\"ok\":true}}\r\n\r".as_slice(),
            ),
            Ok::<_, String>(b"\n".as_slice()),
        ]);
        let result = extract_sse_response_stream(chunks, 7).await.unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn json_http_response_requires_matching_id() {
        let message = json!({"jsonrpc": "2.0", "id": 8, "result": {"ok": true}});
        assert_eq!(
            unpack_jsonrpc_response_for_id(&message, 8).unwrap(),
            json!({"ok": true})
        );
        assert!(unpack_jsonrpc_response_for_id(&message, 9).is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn stdio_frame_reader_rejects_oversized_line() {
        let bytes = vec![b'x'; 65];
        let mut reader = tokio::io::BufReader::new(bytes.as_slice());
        let error = read_line_bounded(&mut reader, 64).await.unwrap_err();
        assert!(error.contains("exceeded 64 bytes"), "{error}");
    }

    #[test]
    fn config_serde_discriminants() {
        let config: McpServerConfig =
            serde_json::from_str(r#"{"type": "stdio", "command": "server-bin", "args": ["--x"]}"#)
                .unwrap();
        assert!(matches!(config, McpServerConfig::Stdio { .. }));
        let config: McpServerConfig =
            serde_json::from_str(r#"{"type": "http", "url": "https://mcp.example.com"}"#).unwrap();
        assert!(matches!(config, McpServerConfig::Http { .. }));
    }

    #[tokio::test]
    async fn failed_server_does_not_block_startup() {
        let mut manager = McpClientManager::new(vec![
            (
                "broken".into(),
                McpServerConfig::Stdio {
                    command: "/nonexistent/mcp-server-binary".into(),
                    args: vec![],
                    env: None,
                    cwd: None,
                },
            ),
            (
                "unsupported".into(),
                McpServerConfig::Ws {
                    url: "wss://x".into(),
                    headers: HashMap::new(),
                },
            ),
        ]);
        manager.connect_all().await;
        let statuses = manager.list_statuses();
        assert_eq!(statuses.len(), 2);
        assert!(
            statuses
                .iter()
                .all(|status| status.state == McpConnectionState::Failed)
        );
        let unsupported = statuses
            .iter()
            .find(|status| status.name == "unsupported")
            .unwrap();
        assert!(unsupported.detail.contains("Unsupported MCP transport"));
        // 未连接 server 的调用报明确错误
        let error = manager
            .call_tool("broken", "x", json!({}))
            .await
            .unwrap_err();
        assert!(error.contains("not connected"));
    }

    /// 进程内伪 MCP server：脚本化 stdio JSON-RPC（用 `sh` + `printf` 太脆，
    /// 直接以内存传输覆盖 manager 逻辑；stdio 传输的真实进程链路留给
    /// 集成测试 `tools_mcp.rs` 用 Python 假 server 覆盖）。
    struct ScriptedTransport {
        tools: Value,
    }

    #[async_trait::async_trait]
    impl McpTransport for ScriptedTransport {
        async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
            match method {
                "initialize" => Ok(json!({"protocolVersion": MCP_PROTOCOL_VERSION})),
                "tools/list" => Ok(self.tools.clone()),
                "tools/call" => {
                    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                    Ok(json!({"content": [
                        {"type": "text", "text": format!("called {name} with {}", params["arguments"])}
                    ]}))
                }
                other => Err(format!("unexpected method {other}")),
            }
        }

        async fn notify(&mut self, _method: &str, _params: Value) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn bridged_tool_dispatches_through_manager() {
        let mut manager = McpClientManager::new(vec![]);
        // 手工注入 scripted session + connected 状态
        let transport: Box<dyn McpTransport> = Box::new(ScriptedTransport {
            tools: json!({"tools": [{"name": "echo", "description": "echo back"}]}),
        });
        manager
            .sessions
            .insert("srv".into(), Arc::new(futures::lock::Mutex::new(transport)));
        manager.statuses.insert(
            "srv".into(),
            McpConnectionStatus {
                name: "srv".into(),
                state: McpConnectionState::Connected,
                detail: String::new(),
                transport: "http".into(),
                auth_configured: false,
                tools: vec![McpToolInfo {
                    server_name: "srv".into(),
                    name: "echo".into(),
                    description: "echo back".into(),
                    input_schema: json!({"type": "object", "properties": {}}),
                }],
            },
        );
        let manager = Arc::new(futures::lock::Mutex::new(manager));

        let mut runtime = crate::tools::ToolRuntime::new();
        register_mcp_tools(&mut runtime, manager).await.unwrap();
        assert_eq!(runtime.len(), 1);
        let tool = runtime.get("mcp__srv__echo").expect("bridged tool present");
        assert_eq!(tool.definition().name, "mcp__srv__echo");

        let mut metadata = crate::tools::ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: std::path::Path::new("/tmp"),
            metadata: &mut metadata,
        };
        let result = tool.execute(json!({"msg": "hi"}), &mut ctx).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output, r#"called echo with {"msg":"hi"}"#);
    }

    #[tokio::test]
    async fn failed_status_removes_and_rejects_stale_session() {
        let mut manager = McpClientManager::new(vec![]);
        let transport: Box<dyn McpTransport> = Box::new(ScriptedTransport {
            tools: json!({"tools": []}),
        });
        manager
            .sessions
            .insert("srv".into(), Arc::new(futures::lock::Mutex::new(transport)));
        manager.statuses.insert(
            "srv".into(),
            McpConnectionStatus {
                name: "srv".into(),
                state: McpConnectionState::Connected,
                detail: String::new(),
                transport: "http".into(),
                auth_configured: false,
                tools: Vec::new(),
            },
        );

        manager.mark_failed(
            "srv",
            &McpServerConfig::Http {
                url: "https://example.invalid/mcp".into(),
                headers: HashMap::new(),
            },
            "reconnect failed".into(),
        );

        assert!(!manager.sessions.contains_key("srv"));
        let error = manager.session_handle("srv").unwrap_err();
        assert!(error.contains("not connected"));
        assert!(error.contains("reconnect failed"));
    }

    #[tokio::test]
    async fn bridged_name_collisions_fail_without_partial_registration() {
        let mut manager = McpClientManager::new(vec![]);
        for (server_name, tool_name) in [("team one", "echo"), ("team!one", "echo")] {
            manager.statuses.insert(
                server_name.into(),
                McpConnectionStatus {
                    name: server_name.into(),
                    state: McpConnectionState::Connected,
                    detail: String::new(),
                    transport: "http".into(),
                    auth_configured: false,
                    tools: vec![McpToolInfo {
                        server_name: server_name.into(),
                        name: tool_name.into(),
                        description: String::new(),
                        input_schema: json!({"type": "object"}),
                    }],
                },
            );
        }
        let manager = Arc::new(futures::lock::Mutex::new(manager));
        let mut runtime = crate::tools::ToolRuntime::new();
        let error = register_mcp_tools(&mut runtime, manager).await.unwrap_err();
        assert!(error.contains("name collision"), "{error}");
        assert!(
            runtime.is_empty(),
            "registration must be atomic on collision"
        );
    }
}
