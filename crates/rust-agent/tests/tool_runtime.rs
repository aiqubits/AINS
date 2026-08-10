//! Phase 3 Tool Runtime 集成测试（Native）：
//! - Kernel 端到端：权限三态（plan 拦截 / 确认回调 / 敏感路径黑名单）、
//!   hook 阻断路径（对照 AINS_PLAN 3.10 验收项）。
//! - ToolRuntime 管线：pre_tool_use 阻断、AlwaysAllow 会话放行、输出预算外置。
//! - MCP stdio 真进程握手 + 工具桥接。
//! - Shell 必经 Sandbox：NoopSandbox 下拒绝执行。

#![cfg(not(target_arch = "wasm32"))]

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use rust_agent::TokioRuntimeAdapter;
use rust_agent::error::ToolError;
use rust_agent::hooks::{
    CommandHookDefinition, HookDefinition, HookEvent, HookExecutor, HookRegistry,
};
use rust_agent::kernel::{
    AgentEvent, AgentKernel, AgentKernelConfig, ScriptedModelClient, StreamEvent,
};
use rust_agent::model_client::UsageSnapshot;
use rust_agent::policy::{
    NoopSandbox, PermissionEngine, PermissionMode, PermissionPrompt, PermissionReply,
    PermissionRequest, PermissionSettings, Sandbox, SandboxCapabilities, SandboxError,
    ShellOutcome, ShellRequest,
};
use rust_agent::tools::mcp::{McpClientManager, McpServerConfig, register_mcp_tools};
use rust_agent::tools::outputs::FsArtifactSink;
use rust_agent::tools::system::{ClipboardTool, ScreenshotTool, ShellCommandTool};
use rust_agent::tools::{
    Tool, ToolCategory, ToolContext, ToolDef, ToolMetadata, ToolResult, ToolRuntime,
};

struct TestShellSandbox;

#[async_trait::async_trait]
impl Sandbox for TestShellSandbox {
    fn name(&self) -> &'static str {
        "test-shell"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            shell: true,
            ..Default::default()
        }
    }

    async fn exec_shell(&self, request: ShellRequest) -> Result<ShellOutcome, SandboxError> {
        use std::process::Stdio;

        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(request.command)
            .current_dir(request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| SandboxError::Execution(error.to_string()))?;
        match tokio::time::timeout(request.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let mut combined = output.stdout;
                combined.extend(output.stderr);
                Ok(ShellOutcome {
                    output: String::from_utf8_lossy(&combined).into_owned(),
                    exit_code: output.status.code(),
                    timed_out: false,
                    cancelled: false,
                })
            }
            Ok(Err(error)) => Err(SandboxError::Execution(error.to_string())),
            Err(_) => Ok(ShellOutcome {
                output: String::new(),
                exit_code: None,
                timed_out: true,
                cancelled: false,
            }),
        }
    }
}

struct WriteMarkerTool;

#[async_trait::async_trait]
impl Tool for WriteMarkerTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write_marker".into(),
            description: "write a marker file".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let rel = input["path"].as_str().unwrap_or("marker.txt");
        let path = ctx.cwd.join(rel);
        std::fs::write(&path, "written").map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolResult::ok(format!("wrote {}", path.display())))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }
}

struct BigOutputTool;

#[async_trait::async_trait]
impl Tool for BigOutputTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "big_output".into(),
            description: "return a huge output".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        _input: Value,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::ok("Z".repeat(20_000)))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Compute
    }
}

fn usage() -> UsageSnapshot {
    UsageSnapshot {
        input_tokens: 1,
        output_tokens: 1,
    }
}

fn user_message(text: &str) -> AgentEvent {
    AgentEvent::UserMessage {
        content: text.into(),
        attachments: vec![],
    }
}

async fn run_kernel_with_runtime(
    model: Arc<ScriptedModelClient>,
    runtime: ToolRuntime,
    config: AgentKernelConfig,
    events: Vec<AgentEvent>,
) -> (AgentKernel<TokioRuntimeAdapter>, Vec<StreamEvent>) {
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::with_runtime(model, runtime, config);
    for event in events {
        event_tx.try_send(event).unwrap();
    }
    drop(event_tx);
    kernel.run().await.unwrap();
    let mut collected = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        collected.push(event);
    }
    (kernel, collected)
}

fn tool_result_events(events: &[StreamEvent]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolExecutionCompleted {
                tool_name,
                output,
                is_error,
                ..
            } => Some((tool_name.clone(), output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

// ── 权限三态端到端 ──────────────────────────────────────────────────────

#[tokio::test]
async fn plan_mode_blocks_mutating_tool_via_kernel_loop() {
    let dir = tempfile::tempdir().unwrap();
    let engine = PermissionEngine::new(PermissionMode::Plan, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    runtime.register(Box::new(WriteMarkerTool));

    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                Some("写入文件"),
                "tu_1",
                "write_marker",
                json!({"path": "marker.txt"}),
            ),
            usage(),
        ),
        ScriptedModelClient::text_turn("已被拦截", usage()),
    ]));
    let config = AgentKernelConfig {
        cwd: dir.path().to_path_buf(),
        idle_timeout: Duration::from_secs(5),
        ..AgentKernelConfig::default()
    };
    let (_kernel, events) =
        run_kernel_with_runtime(model, runtime, config, vec![user_message("写标记文件")]).await;

    let results = tool_result_events(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].2, "plan 模式下写工具必须报错");
    assert!(results[0].1.contains("Plan mode blocks mutating tools"));
    // 文件确实没有被写入
    assert!(!dir.path().join("marker.txt").exists());
}

struct CountingPrompt {
    reply: PermissionReply,
    calls: AtomicUsize,
}

struct CapturingPrompt {
    request: Mutex<Option<PermissionRequest>>,
}

#[async_trait::async_trait]
impl PermissionPrompt for CapturingPrompt {
    async fn confirm(
        &self,
        request: &PermissionRequest,
        _cancel: Option<Arc<AtomicBool>>,
    ) -> PermissionReply {
        *self.request.lock().unwrap() = Some(request.clone());
        PermissionReply::Deny
    }
}

#[async_trait::async_trait]
impl PermissionPrompt for CountingPrompt {
    async fn confirm(
        &self,
        _request: &PermissionRequest,
        _cancel: Option<Arc<AtomicBool>>,
    ) -> PermissionReply {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.reply
    }
}

#[tokio::test]
async fn default_mode_confirmation_flow_allow_deny_always_allow() {
    let dir = tempfile::tempdir().unwrap();

    // Deny：拒绝后返回决策 reason
    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let prompt = Arc::new(CountingPrompt {
        reply: PermissionReply::Deny,
        calls: AtomicUsize::new(0),
    });
    let mut runtime = ToolRuntime::new().with_permissions(engine, Some(prompt.clone()));
    runtime.register(Box::new(WriteMarkerTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: dir.path(),
        metadata: &mut metadata,
    };
    let tool_use = rust_agent::kernel::ToolUse {
        id: "tu_1".into(),
        name: "write_marker".into(),
        input: json!({"path": "denied.txt"}),
    };
    let result = runtime.dispatch(&tool_use, &mut ctx).await;
    assert!(result.is_error);
    assert!(result.output.contains("require user confirmation"));
    assert_eq!(prompt.calls.load(Ordering::SeqCst), 1);
    assert!(!dir.path().join("denied.txt").exists());

    // AlwaysAllow：首次询问后放行，第二次不再询问（会话级放行集）
    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let prompt = Arc::new(CountingPrompt {
        reply: PermissionReply::AlwaysAllow,
        calls: AtomicUsize::new(0),
    });
    let mut runtime = ToolRuntime::new().with_permissions(engine, Some(prompt.clone()));
    runtime.register(Box::new(WriteMarkerTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: dir.path(),
        metadata: &mut metadata,
    };
    let result = runtime.dispatch(&tool_use, &mut ctx).await;
    assert!(!result.is_error, "{}", result.output);
    let result = runtime.dispatch(&tool_use, &mut ctx).await;
    assert!(!result.is_error);
    assert_eq!(
        prompt.calls.load(Ordering::SeqCst),
        1,
        "AlwaysAllow 后第二次调用不得再询问"
    );
}

#[tokio::test]
async fn permission_prompt_receives_operation_context() {
    let dir = tempfile::tempdir().unwrap();
    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let prompt = Arc::new(CapturingPrompt {
        request: Mutex::new(None),
    });
    let mut runtime = ToolRuntime::new().with_permissions(engine, Some(prompt.clone()));
    runtime.register(Box::new(WriteMarkerTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: dir.path(),
        metadata: &mut metadata,
    };
    let input = json!({"path": "nested/marker.txt", "content": "visible to UI"});
    let result = runtime
        .dispatch(
            &rust_agent::kernel::ToolUse {
                id: "tu_context".into(),
                name: "write_marker".into(),
                input: input.clone(),
            },
            &mut ctx,
        )
        .await;
    assert!(result.is_error);
    let request = prompt.request.lock().unwrap().clone().unwrap();
    assert_eq!(request.tool_input, input);
    assert_eq!(request.command, None);
    assert_eq!(
        request.resolved_file_path.as_deref(),
        Some(dir.path().join("nested/marker.txt").to_str().unwrap())
    );
}

#[tokio::test]
async fn privacy_sensitive_reads_require_confirmation() {
    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let prompt = Arc::new(CountingPrompt {
        reply: PermissionReply::Deny,
        calls: AtomicUsize::new(0),
    });
    let mut runtime = ToolRuntime::new().with_permissions(engine, Some(prompt.clone()));
    runtime.register(Box::new(ClipboardTool::new(None)));
    runtime.register(Box::new(ScreenshotTool::new(None)));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/tmp"),
        metadata: &mut metadata,
    };

    for tool_use in [
        rust_agent::kernel::ToolUse {
            id: "tu_clipboard".into(),
            name: "clipboard".into(),
            input: json!({"action": "read"}),
        },
        rust_agent::kernel::ToolUse {
            id: "tu_screenshot".into(),
            name: "screenshot".into(),
            input: json!({}),
        },
    ] {
        let result = runtime.dispatch(&tool_use, &mut ctx).await;
        assert!(result.is_error);
        assert!(result.output.contains("require user confirmation"));
    }
    assert_eq!(prompt.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn exit_plan_mode_uses_confirmation_and_restores_default_mode() {
    use rust_agent::tools::interact::ExitPlanModeTool;

    let engine = PermissionEngine::new(PermissionMode::Plan, PermissionSettings::default());
    let prompt = Arc::new(CountingPrompt {
        reply: PermissionReply::Allow,
        calls: AtomicUsize::new(0),
    });
    let mut runtime = ToolRuntime::new().with_permissions(engine.clone(), Some(prompt.clone()));
    runtime.register(Box::new(ExitPlanModeTool::new(engine.clone())));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/tmp"),
        metadata: &mut metadata,
    };
    let result = runtime
        .dispatch(
            &rust_agent::kernel::ToolUse {
                id: "tu_exit_plan".into(),
                name: "exit_plan_mode".into(),
                input: Value::Null,
            },
            &mut ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(prompt.calls.load(Ordering::SeqCst), 1);
    assert_eq!(engine.mode(), PermissionMode::Default);
}

#[tokio::test]
async fn sensitive_path_blacklist_denies_even_in_full_auto() {
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    runtime.register(Box::new(WriteMarkerTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/home/user"),
        metadata: &mut metadata,
    };
    let tool_use = rust_agent::kernel::ToolUse {
        id: "tu_1".into(),
        name: "write_marker".into(),
        input: json!({"path": "/home/user/.aws/credentials"}),
    };
    let result = runtime.dispatch(&tool_use, &mut ctx).await;
    assert!(result.is_error);
    assert!(result.output.contains("sensitive credential path"));
}

// ── Hook 阻断路径 ───────────────────────────────────────────────────────

#[tokio::test]
async fn pre_tool_use_hook_blocks_dispatch_via_kernel_loop() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = HookRegistry::new();
    registry.register(
        HookEvent::PreToolUse,
        HookDefinition::Command(CommandHookDefinition {
            command: "echo forbidden-by-hook; exit 1".into(),
            timeout_seconds: 10,
            matcher: Some("write_*".into()),
            block_on_failure: true,
            priority: 0,
        }),
    );
    let hooks = Arc::new(
        HookExecutor::new(registry, dir.path().to_path_buf())
            .with_sandbox(Arc::new(TestShellSandbox)),
    );
    let mut runtime = ToolRuntime::new().with_hooks(hooks);
    runtime.register(Box::new(WriteMarkerTool));

    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::turn(
            ScriptedModelClient::assistant_tool_use(
                None,
                "tu_1",
                "write_marker",
                json!({"path": "hooked.txt"}),
            ),
            usage(),
        ),
        ScriptedModelClient::text_turn("被 hook 拦截", usage()),
    ]));
    let config = AgentKernelConfig {
        cwd: dir.path().to_path_buf(),
        idle_timeout: Duration::from_secs(5),
        ..AgentKernelConfig::default()
    };
    let (_kernel, events) =
        run_kernel_with_runtime(model, runtime, config, vec![user_message("写")]).await;

    let results = tool_result_events(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].2);
    assert!(results[0].1.contains("forbidden-by-hook"));
    assert!(!dir.path().join("hooked.txt").exists());
}

#[tokio::test]
async fn non_matching_hook_does_not_block() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = HookRegistry::new();
    registry.register(
        HookEvent::PreToolUse,
        HookDefinition::Command(CommandHookDefinition {
            command: "exit 1".into(),
            timeout_seconds: 10,
            matcher: Some("other_tool".into()),
            block_on_failure: true,
            priority: 0,
        }),
    );
    let hooks = Arc::new(HookExecutor::new(registry, dir.path().to_path_buf()));
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new()
        .with_permissions(engine, None)
        .with_hooks(hooks);
    runtime.register(Box::new(WriteMarkerTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: dir.path(),
        metadata: &mut metadata,
    };
    let tool_use = rust_agent::kernel::ToolUse {
        id: "tu_1".into(),
        name: "write_marker".into(),
        input: json!({"path": "ok.txt"}),
    };
    let result = runtime.dispatch(&tool_use, &mut ctx).await;
    assert!(!result.is_error, "{}", result.output);
    assert!(dir.path().join("ok.txt").exists());
}

#[tokio::test]
async fn blocked_hook_output_still_uses_tool_output_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = HookRegistry::new();
    registry.register(
        HookEvent::PreToolUse,
        HookDefinition::Command(CommandHookDefinition {
            command: "yes x | head -c 30000; exit 1".into(),
            timeout_seconds: 10,
            matcher: Some("write_*".into()),
            block_on_failure: true,
            priority: 0,
        }),
    );
    let hooks = Arc::new(
        HookExecutor::new(registry, dir.path().to_path_buf())
            .with_sandbox(Arc::new(TestShellSandbox)),
    );
    let mut runtime = ToolRuntime::new().with_hooks(hooks);
    runtime.register(Box::new(WriteMarkerTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: dir.path(),
        metadata: &mut metadata,
    };
    let result = runtime
        .dispatch(
            &rust_agent::kernel::ToolUse {
                id: "tu_budget".into(),
                name: "write_marker".into(),
                input: json!({"path": "blocked.txt"}),
            },
            &mut ctx,
        )
        .await;
    assert!(result.is_error);
    assert!(result.output.starts_with("[Tool output truncated]"));
    assert!(!dir.path().join("blocked.txt").exists());
}

// ── 输出预算外置 ────────────────────────────────────────────────────────

#[tokio::test]
async fn oversized_tool_output_offloads_to_artifact_sink() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(FsArtifactSink::new(dir.path().join("artifacts")));
    let mut runtime = ToolRuntime::new().with_artifact_sink(sink);
    runtime.register(Box::new(BigOutputTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: dir.path(),
        metadata: &mut metadata,
    };
    let tool_use = rust_agent::kernel::ToolUse {
        id: "tu_big".into(),
        name: "big_output".into(),
        input: Value::Null,
    };
    let result = runtime.dispatch(&tool_use, &mut ctx).await;
    assert!(!result.is_error);
    assert!(result.output.starts_with("[Tool output truncated]"));
    assert!(result.output.contains("Original size: 20000 chars"));
    // 全文落盘且独立的活跃工件列表记录了引用（不占 work_log 配额）
    assert!(metadata.work_log.is_empty());
    let artifact_path = metadata
        .active_artifacts
        .first()
        .expect("artifact reference in active_artifacts");
    assert_eq!(
        std::fs::read_to_string(artifact_path).unwrap(),
        "Z".repeat(20_000)
    );
}

// ── Shell 必经 Sandbox ─────────────────────────────────────────────────

#[tokio::test]
async fn shell_command_via_noop_sandbox_is_refused_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    // full_auto 也不能让 shell 绕过沙箱层
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    runtime.register(Box::new(ShellCommandTool::new(Arc::new(NoopSandbox))));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: dir.path(),
        metadata: &mut metadata,
    };
    let tool_use = rust_agent::kernel::ToolUse {
        id: "tu_sh".into(),
        name: "shell_command".into(),
        input: json!({"command": "touch should-not-exist.txt"}),
    };
    let result = runtime.dispatch(&tool_use, &mut ctx).await;
    assert!(result.is_error);
    assert!(result.output.contains("无操作权限"), "{}", result.output);
    assert!(!dir.path().join("should-not-exist.txt").exists());
}

// ── MCP stdio 真进程握手 + 桥接 ────────────────────────────────────────

/// 伪 MCP server：预先吐出 initialize / tools/list / tools/call 三个响应
/// （newline-delimited JSON-RPC），之后吞掉 stdin 保持存活。
const FAKE_MCP_SERVER: &str = r#"
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"reply pong","inputSchema":{"type":"object","properties":{}}}]}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"pong"}]}}'
cat > /dev/null
"#;

#[tokio::test]
async fn mcp_stdio_server_connects_and_bridges_tool() {
    let mut manager = McpClientManager::new(vec![(
        "fake".into(),
        McpServerConfig::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), FAKE_MCP_SERVER.into()],
            env: None,
            cwd: None,
        },
    )]);
    manager.connect_all().await;
    {
        let statuses = manager.list_statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(
            statuses[0].state,
            rust_agent::tools::mcp::McpConnectionState::Connected,
            "detail: {}",
            statuses[0].detail
        );
        assert_eq!(statuses[0].tools.len(), 1);
        assert_eq!(statuses[0].tools[0].name, "ping");
    }

    let manager = Arc::new(futures::lock::Mutex::new(manager));
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    register_mcp_tools(&mut runtime, manager).await.unwrap();
    assert_eq!(runtime.len(), 1);

    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/tmp"),
        metadata: &mut metadata,
    };
    let tool_use = rust_agent::kernel::ToolUse {
        id: "tu_mcp".into(),
        name: "mcp__fake__ping".into(),
        input: json!({}),
    };
    let result = runtime.dispatch(&tool_use, &mut ctx).await;
    assert!(!result.is_error, "{}", result.output);
    assert_eq!(result.output, "pong");
}

// ── 全家桶注册冒烟：内置工具集与 schema 下发 ───────────────────────────

#[tokio::test]
async fn dispatch_many_serializes_same_file_write_aliases() {
    // review 十三轮修复回归：同一批次内经相对/绝对路径别名命中
    // 同一文件的两次 edit_file 必须触发独占键冲突→顺序执行，
    // 两次编辑都不得丢失。
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.txt"), "alpha beta\n").unwrap();
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    rust_agent::tools::filesystem::register_filesystem_tools(&mut runtime);

    let absolute_alias = dir.path().join("sub/../doc.txt");
    let mut metadata = ToolMetadata::new();
    let results = runtime
        .dispatch_many(
            &[
                rust_agent::kernel::ToolUse {
                    id: "tu_edit_1".into(),
                    name: "edit_file".into(),
                    input: json!({"path": "doc.txt", "old_str": "alpha", "new_str": "ALPHA"}),
                },
                rust_agent::kernel::ToolUse {
                    id: "tu_edit_2".into(),
                    name: "edit_file".into(),
                    input: json!({
                        "path": absolute_alias.to_string_lossy(),
                        "old_str": "beta",
                        "new_str": "BETA",
                    }),
                },
            ],
            dir.path(),
            &mut metadata,
        )
        .await;
    assert_eq!(results.len(), 2);
    for result in &results {
        assert!(!result.is_error, "{}", result.output);
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("doc.txt")).unwrap(),
        "ALPHA BETA\n",
        "both edits must survive the batch"
    );
}

#[tokio::test]
async fn dispatch_many_mixed_batch_isolates_failure_and_merges_metadata() {
    // review 十四轮补测：N=8 混合读写批次，多个写入命中同一独占键
    //（含相对/别名路径）触发顺序执行；单个失败不得波及其余结果，
    // 所有成功编辑都不丢，metadata 按 tool_use 顺序确定性合并。
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.txt"), "one two three\n").unwrap();
    std::fs::write(dir.path().join("read1.txt"), "r1\n").unwrap();
    std::fs::write(dir.path().join("read2.txt"), "r2\n").unwrap();
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    rust_agent::tools::filesystem::register_filesystem_tools(&mut runtime);
    rust_agent::tools::compute::register_compute_tools(&mut runtime);

    let absolute_alias = dir.path().join("sub/../doc.txt");
    let make = |id: &str, name: &str, input: Value| rust_agent::kernel::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    };
    let tool_uses = [
        make(
            "tu_1",
            "edit_file",
            json!({"path": "doc.txt", "old_str": "one", "new_str": "ONE"}),
        ),
        make("tu_2", "read_file", json!({"path": "read1.txt"})),
        make(
            "tu_3",
            "edit_file",
            json!({
                "path": absolute_alias.to_string_lossy(),
                "old_str": "two",
                "new_str": "TWO",
            }),
        ),
        make("tu_4", "calculator", json!({"expression": "6 * 7"})),
        make(
            "tu_5",
            "edit_file",
            json!({"path": "doc.txt", "old_str": "absent-needle", "new_str": "X"}),
        ),
        make("tu_6", "read_file", json!({"path": "read2.txt"})),
        make(
            "tu_7",
            "edit_file",
            json!({"path": "./doc.txt", "old_str": "three", "new_str": "THREE"}),
        ),
        make("tu_8", "read_file", json!({"path": "doc.txt"})),
    ];

    let mut metadata = ToolMetadata::new();
    let results = runtime
        .dispatch_many(&tool_uses, dir.path(), &mut metadata)
        .await;
    assert_eq!(results.len(), 8);
    for (index, result) in results.iter().enumerate() {
        if index == 4 {
            assert!(result.is_error, "tu_5 must fail: {}", result.output);
        } else {
            assert!(
                !result.is_error,
                "tu_{} failed: {}",
                index + 1,
                result.output
            );
        }
    }
    assert_eq!(results[3].output, "42");
    // 失败的 tu_5 不得影响其余三次编辑
    assert_eq!(
        std::fs::read_to_string(dir.path().join("doc.txt")).unwrap(),
        "ONE TWO THREE\n",
        "all successful edits must survive the batch"
    );
    // 独占键冲突→整批顺序执行，末位读取看到全部编辑结果
    assert!(results[7].output.contains("ONE TWO THREE"));
    // metadata 按 tool_use 顺序确定性合并
    let read_files: Vec<&str> = metadata.read_files.iter().map(String::as_str).collect();
    assert_eq!(read_files.len(), 3, "read_files: {read_files:?}");
    assert!(read_files[0].ends_with("read1.txt"), "{read_files:?}");
    assert!(read_files[1].ends_with("read2.txt"), "{read_files:?}");
    assert!(read_files[2].ends_with("doc.txt"), "{read_files:?}");
}

#[tokio::test]
async fn builtin_toolset_registration_smoke() {
    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine.clone(), None);
    rust_agent::tools::compute::register_compute_tools(&mut runtime);
    rust_agent::tools::filesystem::register_filesystem_tools(&mut runtime);
    rust_agent::tools::system::register_system_tools(&mut runtime, Arc::new(NoopSandbox), None);
    runtime.register(Box::new(rust_agent::tools::network::WebFetchTool::default()));
    runtime.register(Box::new(rust_agent::tools::interact::TodoWriteTool));
    runtime.register(Box::new(
        rust_agent::tools::interact::AskUserQuestionTool::new(None),
    ));
    runtime.register(Box::new(
        rust_agent::tools::interact::EnterPlanModeTool::new(engine.clone()),
    ));
    runtime.register(Box::new(
        rust_agent::tools::interact::ExitPlanModeTool::new(engine),
    ));

    let schemas = runtime.api_schemas();
    // compute 5 + filesystem 5 + system 4 + network 1 + interact 4
    assert_eq!(schemas.len(), 19);
    for def in &schemas {
        assert!(!def.name.is_empty());
        assert!(!def.description.is_empty());
        assert!(def.input_schema.is_object(), "{} schema", def.name);
    }
}
