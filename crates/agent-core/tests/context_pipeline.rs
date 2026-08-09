//! Phase 5.6 汇合集成测试（Native）：验证 Phase 4（感知）与 Phase 5（传输/
//! 提示/持久化/压缩）两线的汇合点。
//!
//! - 感知 → ContextStore → 会话持久化往返（4.3/4.4 ∥ 5.4）；
//! - Kernel 自动压缩后续聊（5.5 接线：Querying 起始触发 → CompactProgress → 续答）；
//! - 分段系统提示各段开关（5.3）。

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::time::Duration;

use agent_core::TokioRuntimeAdapter;
use agent_core::context::compact::estimate_message_tokens;
use agent_core::context::prompt_pipeline::{
    PromptPipelineInput, PromptSections, build_system_prompt,
};
use agent_core::context::{SessionSaveInput, SessionStore};
use agent_core::kernel::context::ContextStore;
use agent_core::kernel::{
    AgentEvent, AgentKernel, AgentKernelConfig, ConversationMessage, Role, ScriptedModelClient,
    StreamEvent, SystemEventType,
};
use agent_core::memory::{KvStore, RedbKvStore};
use agent_core::model_client::UsageSnapshot;
use agent_core::perception::FileChannel;
use agent_core::policy::PermissionMode;
use tempfile::TempDir;

fn kv() -> (Arc<dyn KvStore>, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.redb");
    let store = Arc::new(RedbKvStore::open(&path).unwrap()) as Arc<dyn KvStore>;
    (store, dir)
}

/// 4.3/4.4 ∥ 5.4：拖拽文件 → 感知解析 → 落入 ContextStore → 会话快照往返。
#[tokio::test]
async fn perception_file_into_context_then_session_roundtrip() {
    // 1. 感知：拖拽一个 Markdown 文件
    let outcome = FileChannel::new()
        .ingest(
            b"# Design\nUse a state machine for the agent loop.".to_vec(),
            "design.md",
            None,
        )
        .unwrap();

    // 2. 落入 ContextStore（4.4）
    let mut ctx = ContextStore::new();
    let applied = outcome
        .apply_to_context(&mut ctx, Some("review this design"))
        .await
        .unwrap();
    assert!(applied);
    assert_eq!(ctx.conversation.len(), 1);
    let injected = ctx.conversation[0].text();
    assert!(injected.contains("review this design"));
    assert!(injected.contains("[file: design.md]"));
    assert!(injected.contains("state machine"));

    // 3. 会话持久化（5.4）：从上下文保存快照
    let (kv_store, _temp_dir) = kv();
    let store = SessionStore::new(kv_store);
    let session_id = store
        .save(SessionSaveInput {
            cwd: "/proj/agent".into(),
            model: Some("gpt-test".into()),
            messages: ctx.conversation.clone(),
            tool_metadata: ctx.tool_metadata.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    // 4. 回载：内容保留，summary 取首条 user 文本
    let loaded = store.load_latest("/proj/agent").await.unwrap().unwrap();
    assert_eq!(loaded.session_id, session_id);
    assert_eq!(loaded.messages.len(), 1);
    assert!(loaded.messages[0].text().contains("state machine"));
    assert!(loaded.summary.contains("review this design"));
}

/// 5.5 接线：预载超阈值上下文，提交新输入 → Querying 起始自动压缩 →
/// CompactProgress 事件 → 压缩后续答（260 条 ~2401 字符纯文本消息命中
/// 第 2 级文本折叠即达标，无 LLM 调用；层级由下方 phase 断言钉住）。
#[tokio::test]
async fn kernel_auto_compacts_then_answers() {
    // 脚本：仅需 1 个回答 turn（第 2 级折叠不调用 LLM）
    let model = Arc::new(ScriptedModelClient::new(vec![
        ScriptedModelClient::text_turn("here is the answer", UsageSnapshot::default()),
    ]));
    let config = AgentKernelConfig {
        idle_timeout: Duration::from_secs(5),
        ..AgentKernelConfig::default()
    };
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::new(model.clone(), vec![], config);

    // 预载超自动压缩阈值（167000 tokens）的历史：260 条 × ~2400 字符
    let filler = "detail ".repeat(343); // ~2401 chars
    let mut history = Vec::new();
    for i in 0..260 {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        history.push(ConversationMessage {
            role,
            content: vec![agent_core::kernel::ContentBlock::Text {
                text: format!("turn {i}: {filler}"),
            }],
        });
    }
    let preloaded_tokens = estimate_message_tokens(&history);
    kernel.context_mut().conversation = history;
    kernel
        .context_mut()
        .tool_metadata
        .record_active_artifact("artifact://created-before-compaction");

    // 提交新输入 + 关闭
    event_tx
        .try_send(AgentEvent::UserMessage {
            content: "given all the above, answer now".into(),
            attachments: vec![],
        })
        .unwrap();
    event_tx
        .try_send(AgentEvent::SystemEvent {
            event_type: SystemEventType::Shutdown,
        })
        .unwrap();
    drop(event_tx);

    kernel.run().await.unwrap();

    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }

    // 断言 1：发生了压缩，且降级链停在第 2 级文本折叠（钉住层级：
    // 不应触及第 3 级会话记忆、更不应进入第 4 级 LLM 摘要）
    let phases: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::CompactProgress { phase, .. } => Some(phase.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        phases.contains(&"context_collapse_end"),
        "expected level-2 text collapse, phases: {phases:?}"
    );
    assert!(
        !phases.contains(&"session_memory_end") && !phases.contains(&"compact_start"),
        "compaction must stop at level 2, phases: {phases:?}"
    );

    // Compacted is a completion trigger, not a replacement conversation
    // snapshot. The host retains its full mirror for session persistence and
    // checkpoint/extraction, while Kernel uses the compacted context below for
    // its next model request.
    let compacted_metadata = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::Compacted { tool_metadata, .. } => Some(tool_metadata),
            _ => None,
        })
        .expect("expected completed-compaction snapshot event");
    assert_eq!(
        compacted_metadata.active_artifacts,
        ["artifact://created-before-compaction"],
        "Compacted must expose the current metadata for the host checkpoint"
    );
    assert!(
        estimate_message_tokens(&model.recorded_requests()[0].messages) < preloaded_tokens,
        "Kernel must still use its reduced context for the next model turn"
    );

    // 断言 2：压缩后仍完成回答
    let answered = events.iter().any(|e| matches!(
        e,
        StreamEvent::AssistantTurnComplete { message, .. } if message.text() == "here is the answer"
    ));
    assert!(answered, "expected assistant answer after compaction");

    // 断言 3：会话历史 token 量被压缩（第 2 级文本折叠减少字节而非条数）
    assert!(
        estimate_message_tokens(&kernel.context().conversation) < preloaded_tokens,
        "conversation tokens should shrink after compaction"
    );

    // 断言 4：模型仍被调用（回答 turn）
    assert_eq!(model.recorded_requests().len(), 1);
}

/// 5.5 接线回归（评审建议测试）：Querying 起始的内联自动压缩不重置
/// turn 计数，工具循环不能借由每轮触发压缩绕过 max_turns 预算。
#[tokio::test]
async fn kernel_auto_compact_does_not_reset_turn_budget() {
    // 脚本：turn 0 请求未知工具（回填 error tool_result 后进入 turn 1）
    let model = Arc::new(ScriptedModelClient::new(vec![ScriptedModelClient::turn(
        ScriptedModelClient::assistant_tool_use(None, "c1", "unknown_tool", serde_json::json!({})),
        UsageSnapshot::default(),
    )]));
    let config = AgentKernelConfig {
        idle_timeout: Duration::from_secs(5),
        max_turns: 1,
        ..AgentKernelConfig::default()
    };
    let (mut kernel, mut event_tx, mut stream_rx) =
        AgentKernel::<TokioRuntimeAdapter>::new(model.clone(), vec![], config);

    // 预载超自动压缩阈值的历史（同上一用例：第 2 级文本折叠可达标）
    let filler = "detail ".repeat(343);
    let mut history = Vec::new();
    for i in 0..260 {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        history.push(ConversationMessage {
            role,
            content: vec![agent_core::kernel::ContentBlock::Text {
                text: format!("turn {i}: {filler}"),
            }],
        });
    }
    kernel.context_mut().conversation = history;

    event_tx
        .try_send(AgentEvent::UserMessage {
            content: "do the task".into(),
            attachments: vec![],
        })
        .unwrap();
    event_tx
        .try_send(AgentEvent::SystemEvent {
            event_type: SystemEventType::Shutdown,
        })
        .unwrap();
    drop(event_tx);

    kernel.run().await.unwrap();

    let mut events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        events.push(event);
    }

    // 断言 1：turn 0 确实发生了自动压缩
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::CompactProgress { .. })),
        "expected auto-compaction to fire at turn 0"
    );
    // 断言 2：工具回填后进入 turn 1 即触顶 max_turns（1）——压缩未重置
    // 轮数预算（若被重置，脚本耗尽后将以模型错误而非轮数超限收敛）
    assert!(
        events.iter().any(|e| matches!(
            e,
            StreamEvent::Error { message, recoverable: true } if message.contains("maximum turn limit")
        )),
        "expected max-turns error at turn 1, events: {events:?}"
    );
    // 断言 3：模型仅被调用 1 次（turn 0），turn 1 在调用前被预算拦截
    assert_eq!(model.recorded_requests().len(), 1);
}

/// 5.3：分段系统提示各段开关（对照 test_prompts 用例）。
#[test]
fn system_prompt_section_toggles() {
    let input = PromptPipelineInput {
        cwd: std::path::Path::new("/tmp/ains-none-xyz"),
        now_ms: 1_609_459_200_000,
        permission_mode: PermissionMode::Plan,
        skills: &[],
        memory_prompt: Some("# Memory\n- kv://memdir".to_string()),
        custom_base: None,
        environment_shell: None,
        environment_git_branch: None,
    };

    // 全开：含 base + 权限模式(Plan) + Environment + Memory
    let full = build_system_prompt(&input, PromptSections::default());
    assert!(full.contains("You are AINS"));
    assert!(full.contains("Plan mode is enabled"));
    assert!(full.contains("# Environment"));
    assert!(full.contains("# Memory"));

    // 仅权限模式段
    let only_perm = build_system_prompt(
        &input,
        PromptSections {
            base: false,
            permission_mode: true,
            skills: false,
            project_docs: false,
            environment: false,
            memory: false,
        },
    );
    assert!(only_perm.starts_with("# Current Permission Mode"));
    assert!(!only_perm.contains("You are AINS"));
    assert!(!only_perm.contains("# Memory"));
}
