//! Phase 3 Tool Runtime 契约测试（Web / WASM）。
//!
//! 仅在 CI 中通过 `wasm-pack test --headless --chrome` 执行；覆盖双 target
//! 共享的行为契约：ToolRuntime 分发管线、三态权限（plan 拦截 / 敏感路径
//! 黑名单）、hook matcher 与聚合阻断、输出预算（Web 无 sink 时仅留预览）、
//! 计算工具、todo_write 状态袋回退、command hook 在 Web 上的降级语义。

#![cfg(target_arch = "wasm32")]
// WASM 单线程：HookExecutor 在 wasm 上不满足 Send+Sync 属预期，Arc 仅做引用计数
#![allow(clippy::arc_with_non_send_sync)]

use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{Map, Value, json};
use wasm_bindgen_test::*;

use rust_agent::hooks::{
    CommandHookDefinition, HookDefinition, HookEvent, HookExecutor, HookRegistry,
    PromptHookDefinition,
};
use rust_agent::kernel::ToolUse;
use rust_agent::model_client::{EventStream, ModelClient, ModelRequest, ModelStreamEvent};
use rust_agent::policy::{PermissionEngine, PermissionMode, PermissionSettings};
use rust_agent::tools::compute::{CalculatorTool, register_compute_tools};
use rust_agent::tools::interact::TodoWriteTool;
use rust_agent::tools::outputs::DEFAULT_TOOL_OUTPUT_INLINE_CHARS;
use rust_agent::tools::{Tool, ToolContext, ToolMetadata, ToolRuntime};

wasm_bindgen_test_configure!(run_in_browser);

fn tool_use(name: &str, input: Value) -> ToolUse {
    ToolUse {
        id: "tu_web".into(),
        name: name.into(),
        input,
    }
}

#[wasm_bindgen_test]
async fn compute_tools_work_in_browser() {
    let mut runtime = ToolRuntime::new();
    register_compute_tools(&mut runtime);
    assert_eq!(runtime.len(), 5);

    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/"),
        metadata: &mut metadata,
    };
    let result = runtime
        .dispatch(
            &tool_use("calculator", json!({"expression": "(2 + 3) * 4"})),
            &mut ctx,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(result.output, "20");

    let result = runtime
        .dispatch(
            &tool_use(
                "json",
                json!({"action": "get", "json": r#"{"a":[1,2]}"#, "pointer": "/a/0"}),
            ),
            &mut ctx,
        )
        .await;
    assert_eq!(result.output, "1");
}

#[wasm_bindgen_test]
async fn plan_mode_and_sensitive_paths_enforced_on_web() {
    let engine = PermissionEngine::new(PermissionMode::Plan, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine.clone(), None);
    runtime.register(Box::new(TodoWriteTool));

    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/"),
        metadata: &mut metadata,
    };
    // plan 模式：非只读工具拒绝
    let result = runtime
        .dispatch(&tool_use("todo_write", json!({"item": "x"})), &mut ctx)
        .await;
    assert!(result.is_error);
    assert!(result.output.contains("Plan mode blocks mutating tools"));

    // 敏感路径黑名单在逻辑层同样生效（Web 无本地文件系统仍保持契约一致）
    engine.set_mode(PermissionMode::FullAuto);
    let result = runtime
        .dispatch(
            &tool_use(
                "todo_write",
                json!({"item": "x", "path": "/home/u/.ssh/notes.md"}),
            ),
            &mut ctx,
        )
        .await;
    assert!(result.is_error);
    assert!(result.output.contains("sensitive credential path"));
}

#[wasm_bindgen_test]
async fn todo_write_falls_back_to_metadata_bag_on_web() {
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    runtime.register(Box::new(TodoWriteTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/"),
        metadata: &mut metadata,
    };
    let result = runtime
        .dispatch(
            &tool_use("todo_write", json!({"item": "web item"})),
            &mut ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.output);
    let stored = metadata
        .extra
        .get("todo_markdown:TODO.md")
        .and_then(Value::as_str)
        .expect("todo checklist persisted in metadata bag");
    assert_eq!(stored, "# TODO\n- [ ] web item\n");
    // 标记完成走同一状态袋
    let mut ctx = ToolContext {
        cwd: Path::new("/"),
        metadata: &mut metadata,
    };
    runtime
        .dispatch(
            &tool_use("todo_write", json!({"item": "web item", "checked": true})),
            &mut ctx,
        )
        .await;
    let stored = metadata
        .extra
        .get("todo_markdown:TODO.md")
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(stored, "# TODO\n- [x] web item\n");
}

#[wasm_bindgen_test]
async fn concurrent_todo_writes_to_same_document_preserve_both_updates() {
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    runtime.register(Box::new(TodoWriteTool));
    let mut metadata = ToolMetadata::new();
    let results = runtime
        .dispatch_many(
            &[
                tool_use("todo_write", json!({"item": "first"})),
                tool_use("todo_write", json!({"item": "second"})),
            ],
            Path::new("/"),
            &mut metadata,
        )
        .await;
    assert!(results.iter().all(|result| !result.is_error));
    assert_eq!(
        metadata.extra["todo_markdown:TODO.md"],
        Value::String("# TODO\n- [ ] first\n- [ ] second\n".into())
    );
}

#[wasm_bindgen_test]
async fn concurrent_todo_path_aliases_preserve_one_document() {
    let engine = PermissionEngine::new(PermissionMode::FullAuto, PermissionSettings::default());
    let mut runtime = ToolRuntime::new().with_permissions(engine, None);
    runtime.register(Box::new(TodoWriteTool));
    let mut metadata = ToolMetadata::new();
    let results = runtime
        .dispatch_many(
            &[
                tool_use("todo_write", json!({"item": "first", "path": "TODO.md"})),
                tool_use("todo_write", json!({"item": "second", "path": "./TODO.md"})),
            ],
            Path::new("/"),
            &mut metadata,
        )
        .await;
    assert!(results.iter().all(|result| !result.is_error));
    assert_eq!(
        metadata.extra["todo_markdown:TODO.md"],
        Value::String("# TODO\n- [ ] first\n- [ ] second\n".into())
    );
    assert!(!metadata.extra.contains_key("todo_markdown:./TODO.md"));
}

#[wasm_bindgen_test]
async fn oversized_output_keeps_preview_only_without_sink() {
    struct Big;

    #[async_trait::async_trait(?Send)]
    impl Tool for Big {
        fn definition(&self) -> rust_agent::tools::ToolDef {
            rust_agent::tools::ToolDef {
                name: "big".into(),
                description: "big".into(),
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
        ) -> Result<rust_agent::tools::ToolResult, rust_agent::error::ToolError> {
            Ok(rust_agent::tools::ToolResult::ok(
                "W".repeat(DEFAULT_TOOL_OUTPUT_INLINE_CHARS + 7),
            ))
        }

        fn category(&self) -> rust_agent::tools::ToolCategory {
            rust_agent::tools::ToolCategory::Compute
        }
    }

    let mut runtime = ToolRuntime::new();
    runtime.register(Box::new(Big));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/"),
        metadata: &mut metadata,
    };
    let result = runtime
        .dispatch(&tool_use("big", Value::Null), &mut ctx)
        .await;
    assert!(!result.is_error);
    assert!(result.output.starts_with("[Tool output truncated]"));
    assert!(result.output.contains("no artifact storage available"));
}

#[wasm_bindgen_test]
async fn command_hooks_degrade_gracefully_on_web() {
    let mut registry = HookRegistry::new();
    registry.register(
        HookEvent::PreToolUse,
        HookDefinition::Command(CommandHookDefinition {
            command: "echo hi".into(),
            timeout_seconds: 5,
            matcher: None,
            block_on_failure: true,
            priority: 0,
        }),
    );
    let executor = HookExecutor::new(registry, "/".into());
    let mut payload = Map::new();
    payload.insert("tool_name".into(), Value::String("calculator".into()));
    let aggregated = executor.execute(HookEvent::PreToolUse, &payload).await;
    // Web 无进程派生：command hook 报失败，block_on_failure=true 时阻断
    assert!(aggregated.blocked());
    assert!(
        aggregated
            .reason()
            .contains("not supported on the web platform")
    );

    // pre_tool_use hook 阻断经由管线传导为 error tool_result
    let mut registry = HookRegistry::new();
    registry.register(
        HookEvent::PreToolUse,
        HookDefinition::Command(CommandHookDefinition {
            command: "echo hi".into(),
            timeout_seconds: 5,
            matcher: None,
            block_on_failure: true,
            priority: 0,
        }),
    );
    let mut runtime =
        ToolRuntime::new().with_hooks(Arc::new(HookExecutor::new(registry, "/".into())));
    runtime.register(Box::new(CalculatorTool));
    let mut metadata = ToolMetadata::new();
    let mut ctx = ToolContext {
        cwd: Path::new("/"),
        metadata: &mut metadata,
    };
    let result = runtime
        .dispatch(
            &tool_use("calculator", json!({"expression": "1"})),
            &mut ctx,
        )
        .await;
    assert!(result.is_error);
}

#[wasm_bindgen_test]
async fn network_guard_syntax_checks_run_on_web() {
    use rust_agent::policy::NetworkPolicy;
    use rust_agent::tools::network::{
        ensure_public_http_url, fetch_public_http_response, validate_http_url,
    };
    assert!(validate_http_url("https://example.com/x").is_ok());
    assert!(validate_http_url("ftp://example.com").is_err());
    assert!(validate_http_url("https://u:p@example.com").is_err());
    // 字面量与本地主机名拒绝（Web 端同样生效；DNS 复核由浏览器兜底）
    assert!(ensure_public_http_url("http://127.0.0.1/").await.is_err());
    assert!(ensure_public_http_url("http://localhost/x").await.is_err());
    assert!(ensure_public_http_url("http://intranet/x").await.is_err());
    let error = fetch_public_http_response("https://example.com/", &NetworkPolicy::default())
        .await
        .expect_err("direct browser fetch must fail closed");
    assert!(error.contains("disabled on the web platform"));
}

struct StalledModel;

#[async_trait::async_trait(?Send)]
impl ModelClient for StalledModel {
    async fn stream_response(
        &self,
        _request: ModelRequest,
    ) -> Result<EventStream<ModelStreamEvent>, rust_agent::error::AgentError> {
        Ok(futures::stream::pending().boxed_local())
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, rust_agent::error::AgentError> {
        Ok(Vec::new())
    }

    async fn stt(&self, _audio_data: &[u8]) -> Result<String, rust_agent::error::AgentError> {
        Ok(String::new())
    }

    async fn tts(&self, _text: &str) -> Result<Vec<u8>, rust_agent::error::AgentError> {
        Ok(Vec::new())
    }
}

#[wasm_bindgen_test]
async fn prompt_hook_timeout_is_enforced_on_web() {
    let mut registry = HookRegistry::new();
    registry.register(
        HookEvent::PreToolUse,
        HookDefinition::Prompt(PromptHookDefinition {
            prompt: "validate".into(),
            model: None,
            timeout_seconds: 0,
            matcher: None,
            block_on_failure: true,
            priority: 0,
        }),
    );
    let executor = HookExecutor::new(registry, "/".into()).with_model(Arc::new(StalledModel), None);
    let aggregated = executor.execute(HookEvent::PreToolUse, &Map::new()).await;
    assert!(aggregated.blocked());
    assert!(aggregated.reason().contains("timed out after 0s"));
}
