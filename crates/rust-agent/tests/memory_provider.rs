//! Kernel 动态 system prompt provider 集成测试（§12 / §20.7 第 4–5 项）。
//!
//! 验证：
//! - Querying 构造 ModelRequest 前 await provider，memory section 注入
//!   base system prompt 与 permission mode section 之间；
//! - provider 失败/无内容返回 `None` 时回落 base + permission mode，
//!   主 Agent 请求仍正常构造（Memory 失败不阻断对话路径）。

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};

use rust_agent::TokioRuntimeAdapter;
use rust_agent::context::prompt_pipeline::permission_mode_section;
use rust_agent::kernel::messages::{ContentBlock, Role};
use rust_agent::kernel::mock_model::ScriptedModelClient;
use rust_agent::kernel::{
    AgentEvent, AgentKernel, AgentKernelConfig, AsyncSystemPromptProvider, ConversationMessage,
    StreamEvent,
};
use rust_agent::model_client::{ModelStreamEvent, UsageSnapshot};
use rust_agent::policy::PermissionMode;

fn assistant_turn(text: &str) -> Vec<ModelStreamEvent> {
    let message = ConversationMessage {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    };
    ScriptedModelClient::turn(message, UsageSnapshot::default())
}

/// 固定返回预设段或 None 的 provider（测试桩）。
struct StaticProvider(Option<String>);

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl AsyncSystemPromptProvider for StaticProvider {
    async fn provide(&self, _messages: &[ConversationMessage]) -> Option<String> {
        self.0.clone()
    }
}

/// 驱动一次查询并返回模型收到的第一个请求。
async fn run_query(
    model: Arc<ScriptedModelClient>,
    provider: Option<Arc<dyn AsyncSystemPromptProvider>>,
) -> rust_agent::model_client::ModelRequest {
    let config = AgentKernelConfig {
        system_prompt: Some("BASE-SYSTEM-PROMPT".to_string()),
        memory_provider: provider,
        idle_timeout: Duration::from_secs(1),
        ..AgentKernelConfig::default()
    };
    let (mut kernel, mut event_tx, mut stream_rx) = AgentKernel::<TokioRuntimeAdapter>::new(
        Arc::clone(&model) as Arc<dyn rust_agent::model_client::ModelClient>,
        vec![],
        config,
    );
    let handle = tokio::spawn(async move {
        let _ = kernel.run().await;
    });
    event_tx
        .send(AgentEvent::UserMessage {
            content: "hello".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // 等待回合自然完成（无工具 → AssistantTurnComplete）
    while let Some(event) = stream_rx.next().await {
        if matches!(event, StreamEvent::AssistantTurnComplete { .. }) {
            break;
        }
    }
    drop(event_tx);
    let _ = handle.await;
    model
        .recorded_requests()
        .into_iter()
        .next()
        .expect("model must receive exactly one request")
}

#[tokio::test]
async fn memory_section_injected_between_base_and_permission_mode() {
    let model = Arc::new(ScriptedModelClient::new(vec![assistant_turn("ok")]));
    let request = run_query(
        Arc::clone(&model),
        Some(Arc::new(StaticProvider(Some("MEMORY-SECTION".to_string())))),
    )
    .await;
    let system_prompt = request.system_prompt.expect("system prompt present");
    // 三段拼装顺序：base → dynamic memory → permission mode（§12）
    let base_pos = system_prompt
        .find("BASE-SYSTEM-PROMPT")
        .expect("base present");
    let memory_pos = system_prompt
        .find("MEMORY-SECTION")
        .expect("memory present");
    let mode_pos = system_prompt
        .find(&permission_mode_section(PermissionMode::Default))
        .expect("permission mode present");
    assert!(base_pos < memory_pos, "base 必须先于 memory section");
    assert!(memory_pos < mode_pos, "permission mode 必须位于最后");
    assert!(
        system_prompt.ends_with(&permission_mode_section(PermissionMode::Default)),
        "permission mode section 必须位于最终 system prompt 的权限段位置"
    );
}

#[tokio::test]
async fn provider_failure_falls_back_to_base_and_permission_mode() {
    let model = Arc::new(ScriptedModelClient::new(vec![assistant_turn("ok")]));
    let request = run_query(Arc::clone(&model), Some(Arc::new(StaticProvider(None)))).await;
    let system_prompt = request.system_prompt.expect("system prompt present");
    assert!(
        system_prompt.contains("BASE-SYSTEM-PROMPT"),
        "provider 失败时 base 必须保留"
    );
    assert!(
        !system_prompt.contains("MEMORY-SECTION"),
        "provider 返回 None 时不得注入 memory section"
    );
    assert!(
        system_prompt.ends_with(&permission_mode_section(PermissionMode::Default)),
        "provider 失败时 permission mode section 仍位于最后"
    );
}

#[tokio::test]
async fn without_provider_prompt_is_base_plus_permission_mode() {
    let model = Arc::new(ScriptedModelClient::new(vec![assistant_turn("ok")]));
    let request = run_query(Arc::clone(&model), None).await;
    let system_prompt = request.system_prompt.expect("system prompt present");
    assert!(system_prompt.contains("BASE-SYSTEM-PROMPT"));
    assert!(
        system_prompt.ends_with(&permission_mode_section(PermissionMode::Default)),
        "未配置 provider 时权限段仍位于最后"
    );
}
