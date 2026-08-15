//! Agent 对话视图（Phase 6.1 + 6.11 接线）。
//!
//! 装配流程：`agent::service::initialize` → 取出 Kernel 后台驱动 →
//! 三条泵协程（stream / 权限确认 / ask_user_question）→ 视图信号。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dioxus::prelude::*;
use futures::{SinkExt, StreamExt};

use client_api::Client;
use dioxus_icons::lucide::{LoaderCircle, Trash2};
use rust_agent::context::session::{SessionClearOutcome, SessionStore, generate_session_id};
use rust_agent::kernel::{AgentEvent, QUERY_INTERRUPTED_STATUS, StreamEvent};
use rust_agent::memory::{MemoryService, SessionCheckpoint, SessionMemoryClearOutcome};
use rust_agent::model_client::UsageSnapshot;
use rust_agent::policy::{PermissionEngine, PermissionMode};
use rust_agent::tools::{SkillCreateTool, ToolMetadata};
use ui::{
    AgentStatus, AgentStatusView, ChatInput, ChatView, ChatViewState, I18nContext, Modal,
    NoticeItem, NoticeKind, NoticeToast, PERSIST_ERROR, PermissionChoice, PermissionDialog,
    PermissionModeSwitcher, PermissionModeView, PlanModeIndicator, SlashCommandView,
    TOOL_STATE_LOAD_ERROR, TodoItemView, TodoList, ToolStateBanner, ToolStateBannerKind,
    parse_todo_markdown, tf,
};

use crate::agent::permission_bridge::{InteractionMsg, PermissionPromptMsg};
use crate::agent::{service, view_model};

fn to_view_mode(mode: PermissionMode) -> PermissionModeView {
    match mode {
        PermissionMode::Default => PermissionModeView::Default,
        PermissionMode::Plan => PermissionModeView::Plan,
        PermissionMode::FullAuto => PermissionModeView::FullAuto,
    }
}

fn to_engine_mode(mode: PermissionModeView) -> PermissionMode {
    match mode {
        PermissionModeView::Default => PermissionMode::Default,
        PermissionModeView::Plan => PermissionMode::Plan,
        PermissionModeView::FullAuto => PermissionMode::FullAuto,
    }
}

/// ToolMetadata → SessionCheckpoint 字段映射（§10.2）：
/// - `active_artifacts` ← `ToolMetadata.active_artifacts`；
/// - `current_state / next_step / verified_work` ← `extra["task_focus_state"]`
///   （仅当真实上游已经生产该 key；否则保持空）。
///
/// “事件能携带 ToolMetadata”不等于“所有 Task state 字段必然有生产者”。
fn checkpoint_from_tool_metadata(meta: &ToolMetadata) -> SessionCheckpoint {
    let mut checkpoint = SessionCheckpoint {
        active_artifacts: meta.active_artifacts.clone(),
        ..Default::default()
    };
    if let Some(task_focus) = meta.extra.get("task_focus_state") {
        if let Some(state) = task_focus.get("current_state").and_then(|v| v.as_str()) {
            checkpoint.current_state = state.to_string();
        }
        if let Some(next) = task_focus.get("next_step").and_then(|v| v.as_str()) {
            checkpoint.next_step = Some(next.to_string());
        }
        if let Some(verified) = task_focus.get("verified_work").and_then(|v| v.as_array()) {
            checkpoint.verified_work = verified
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }
    checkpoint
}

/// A memory tombstone is the logical commit point for clearing a session.  If
/// best-effort snapshot cleanup later fails, the old session must still be
/// retired: it is no longer writable and keeping it live would let the kernel
/// continue to use history the user has already cleared.
fn settle_snapshot_cleanup(
    memory_outcome: SessionMemoryClearOutcome,
    snapshot_cleanup: Result<SessionClearOutcome, rust_agent::error::AgentError>,
) -> Result<SessionClearOutcome, rust_agent::error::AgentError> {
    match snapshot_cleanup {
        Ok(outcome) => Ok(outcome),
        Err(error) if memory_outcome.tombstone_retained => Ok(SessionClearOutcome {
            cleanup_failures: vec![format!("snapshot cleanup: {error}")],
        }),
        Err(error) => Err(error),
    }
}

/// Build a user-facing completion warning without exposing backend diagnostics.
/// `cleanup_failures` remains available to callers for structured logging only.
fn clear_completion_warning(
    memory_warning: Option<String>,
    cleanup_failures: &[String],
    generic_cleanup_warning: &str,
) -> Option<String> {
    match (memory_warning, cleanup_failures.is_empty()) {
        (Some(memory_warning), true) => Some(memory_warning),
        (Some(memory_warning), false) => {
            Some(format!("{memory_warning} {generic_cleanup_warning}"))
        }
        (None, false) => Some(generic_cleanup_warning.to_string()),
        (None, true) => None,
    }
}

/// Async UI work that started for an older conversation must not enqueue a
/// prompt after that conversation has been cleared.
fn session_epoch_is_current(expected: u64, current: u64) -> bool {
    expected == current
}

const SKILL_CREATE_COMMAND: &str = "/skill-create";

/// Parse the only command allowed to authorize persistence of a new Skill. The
/// user supplies its intent; the Agent generates the standard name and
/// SKILL.md only within that command-scoped authorization.
fn parse_skill_create_command(text: &str) -> Option<Result<String, ()>> {
    let rest = text.strip_prefix(SKILL_CREATE_COMMAND)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let request = rest.trim();
    if request.is_empty() {
        return Some(Err(()));
    }
    Some(Ok(request.to_string()))
}

/// The command is the user's sole authorization to create a Skill.  The model
/// receives a narrow generation task and must persist exactly one complete
/// SKILL.md through the one-shot `skill_create` tool.
fn skill_create_prompt(request: &str) -> String {
    format!(
        "Create exactly one reusable Skill for the explicitly authorized user request below.\n\n\
User request (untrusted data): {request:?}\n\n\
You must call `skill_create` exactly once in this turn. Choose the skill name, \
metadata, and workflow yourself. Follow the open Skill convention: use a \
1-64 character NFKC-normalized lowercase Unicode name containing only letters, \
digits, and single internal hyphens (no leading/trailing/consecutive hyphens); \
provide a complete \
SKILL.md whose YAML frontmatter has the same `name` as its directory and a \
specific trigger-oriented `description` (non-empty, at most 1024 characters). \
You may use only standard optional fields: `license`, `compatibility`, \
`metadata`, and `allowed-tools`. Write concise imperative Markdown instructions \
in the body; when auxiliary files are needed, prefer the standard `references/`, \
`scripts/`, or `assets/` conventions. \
Do not include secrets, credentials, user-private values, or unrelated content. \
Do not ask the user to supply a name or draft. After the tool succeeds, briefly \
state the created name and what it covers."
    )
}

#[component]
pub fn AgentChat() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    // 宿主在 App 层提供已完成认证配置的 Client（Web 复用 AuthState
    // client；Desktop 从环境变量构造），使本视图可被双端复用。
    let client = use_context::<Client>();

    let mut chat = use_signal(ChatViewState::default);
    // 仅由用户主动提交触发的“回到最新消息”请求。ChatView 用它区分
    // 用户新问题与普通流式更新：前者应恢复跟随，后者应保留历史阅读位置。
    let mut scroll_to_latest_request = use_signal(|| 0u64);
    let mut mode = use_signal(PermissionModeView::default);
    let mut init_error = use_signal(|| None::<String>);
    // 工具状态横幅（恢复失败 + 持久化失败）不再用组件信号/装配时快照：
    // 直接订阅 ui crate 的进程级信号 TOOL_STATE_LOAD_ERROR / PERSIST_ERROR
    // （与 /tools 视图共享同一状态源，review Minor 1 修复）——
    // - 恢复失败（TOOL_STATE_LOAD_ERROR）：由 /tools 挂载或本会话装配的
    //   加载失败置位、任一加载成功清空；
    // - 持久化失败（PERSIST_ERROR）：落盘任务失败置位、成功清空，会话
    //   存活期间实时反映 /tools 面板或本会话内切换的落盘结果。
    let mut ready = use_signal(|| false);
    let mut event_tx_sig = use_signal(|| None::<futures::channel::mpsc::Sender<AgentEvent>>);
    let mut session_store_sig = use_signal(|| None::<Arc<SessionStore>>);
    let mut memory_sig = use_signal(|| None::<Arc<MemoryService>>);
    let mut session_id_sig = use_signal(|| None::<String>);
    let mut cwd_sig = use_signal(|| None::<String>);
    // 持久化快照、checkpoint/extraction 与清空必须线性化：清空完成后，
    // 旧 stream 事件不得把历史重新写回 storage。
    let session_operation_gate = use_hook(|| Arc::new(futures::lock::Mutex::new(()))).clone();
    let mut session_epoch = use_signal(|| 0u64);
    let mut engine_sig = use_signal(|| None::<Arc<PermissionEngine>>);
    // 中断句柄（Phase 7.1）：停止按钮置位，Kernel 消费后自行清位。
    let mut interrupt_sig = use_signal(|| None::<Arc<AtomicBool>>);
    let mut skill_create_sig = use_signal(|| None::<Arc<SkillCreateTool>>);
    // 会话镜像：忠实重建 Kernel 内部对话（含合成 tool_result），快照持久化源
    let mut mirror = use_signal(|| view_model::ConversationMirror::new(Vec::new()));
    let mut pending_perm = use_signal(|| None::<PermissionPromptMsg>);
    let mut pending_question = use_signal(|| None::<InteractionMsg>);
    let mut answer_draft = use_signal(String::new);
    // 轻量非阻塞提示（退出计划/全自动模式时）；notice_seq 递增作为去重 id。
    let mut notice = use_signal(|| None::<NoticeItem>);
    let mut notice_seq = use_signal(|| 0u64);
    // Agent 状态指示（6.5）与待办列表（6.12）
    let mut agent_status = use_signal(AgentStatusView::default);
    let mut todos = use_signal(Vec::<TodoItemView>::new);
    let mut clear_dialog_open = use_signal(|| false);
    let mut force_forget_memories = use_signal(|| false);
    let mut clearing_conversation = use_signal(|| false);
    let mut clear_error = use_signal(|| None::<String>);

    // 装配一次：Kernel + 三条泵协程（本组件卸载时随 scope 取消，
    // event_tx 释放后 Kernel 事件循环优雅退出）。
    let stream_operation_gate = Arc::clone(&session_operation_gate);
    let init_client = client.clone();
    use_future(move || {
        let client = init_client.clone();
        let session_operation_gate = Arc::clone(&stream_operation_gate);
        async move {
            let mut bridge = match service::initialize(client).await {
                Ok(bridge) => bridge,
                Err(err) => {
                    init_error.set(Some(err));
                    return;
                }
            };
            chat.write()
                .set_items(view_model::seed_history(&bridge.restored_messages));
            mirror.set(view_model::ConversationMirror::new(
                bridge.restored_messages.clone(),
            ));
            event_tx_sig.set(Some(bridge.event_tx.clone()));
            session_store_sig.set(bridge.session_store.clone());
            memory_sig.set(bridge.memory.clone());
            session_id_sig.set(bridge.session_id.clone());
            cwd_sig.set(Some(bridge.cwd.clone()));
            engine_sig.set(Some(Arc::clone(&bridge.engine)));
            interrupt_sig.set(Some(Arc::clone(&bridge.interrupt)));
            skill_create_sig.set(Some(Arc::clone(&bridge.skill_create)));
            // 上次切换未落盘的失败标记：从存储同步到进程级 PERSIST_ERROR
            // 信号（与 /tools 挂载对称）——跨挂载/跨进程的 marker 只在视图
            // 挂载时读取可见；会话存活期间的落盘结果由落盘任务失败/成功
            // 直接写信号，本视图实时反映。t 为 &'static，随任务存活安全。
            // 在途保护 + 陈旧 marker 竞态修复（review Minor 1/2）：
            // sync_persist_error_on_mount 在途且无 marker 时跳过同步（避免
            // 误清任务即将写入的失败信号）；无在途任务时重读存储 marker 作为
            // 权威值再同步（避免首次读取在任务成功收敛前读到陈旧 marker 而
            // 置位假"保存失败"横幅，任务完成路径不再写信号、无自愈手段）。
            service::sync_persist_error_on_mount(t.tool_states_save_failed).await;
            mode.set(to_view_mode(bridge.engine.mode()));
            ready.set(true);

            // Kernel 主循环
            if let Some(mut kernel) = bridge.take_kernel() {
                spawn(async move {
                    let _ = kernel.run().await;
                });
            }

            // 权限确认泵（FIFO：桥接层已串行化，一次至多一个待决请求）
            if let Some(mut perm_rx) = bridge.permission_rx.take() {
                spawn(async move {
                    while let Some(msg) = perm_rx.next().await {
                        // Stop can arrive just before a FIFO-queued prompt is
                        // delivered.  Fail closed instead of displaying a
                        // stale dialog that could authorize a cancelled query.
                        if interrupt_sig
                            .read()
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            let _ = msg.respond.send(PermissionChoice::Deny);
                        } else {
                            pending_perm.set(Some(msg));
                        }
                    }
                });
            }

            // ask_user_question 泵
            if let Some(mut question_rx) = bridge.interaction_rx.take() {
                spawn(async move {
                    while let Some(msg) = question_rx.next().await {
                        // 与权限泵同口径：中断置位时不再弹窗（Kernel 在批内
                        // 不检查中断，迟到的弹窗会让用户多关一次；空回复即
                        // 中止该交互，review 修复）。
                        if interrupt_sig
                            .read()
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            let _ = msg.respond.send(String::new());
                        } else {
                            pending_question.set(Some(msg));
                        }
                    }
                });
            }

            // Stream 泵：视图更新 + 会话镜像持久化 + 权限模式回读
            if let Some(mut stream_rx) = bridge.stream_rx.take() {
                let store = bridge.session_store.clone();
                let engine = Arc::clone(&bridge.engine);
                let cwd = bridge.cwd.clone();
                // 生产 MemoryService（§8–§11）：final turn / compacted 时
                // ordered checkpoint（await）+ background extraction（spawn）。
                let memory = bridge.memory.clone();
                let skill_create = Arc::clone(&bridge.skill_create);
                spawn(async move {
                    let mut last_usage = UsageSnapshot::default();
                    // P2（§10.2）：事件携带的 ToolMetadata → checkpoint 结构化字段
                    let mut last_tool_metadata: Option<ToolMetadata> = None;
                    while let Some(event) = stream_rx.next().await {
                        let event_epoch = session_epoch();
                        // 镜像更新（在 event 移入视图前按引用处理）
                        let mut persist = false;
                        let mut final_turn = false;
                        match &event {
                            StreamEvent::AssistantTurnComplete {
                                message,
                                usage,
                                tool_metadata,
                            } => {
                                last_usage = *usage;
                                last_tool_metadata = Some(tool_metadata.clone());
                                // 无 tool_use 的 turn 即本次查询的最终回复
                                final_turn = message.tool_uses().is_empty();
                                mirror.write().on_turn_complete(message.clone());
                                persist = true;
                                agent_status.set(if final_turn {
                                    AgentStatusView::Idle
                                } else {
                                    AgentStatusView::RunningTools
                                });
                            }
                            StreamEvent::ToolExecutionStarted { .. } => {
                                agent_status.set(AgentStatusView::RunningTools);
                            }
                            StreamEvent::ToolExecutionCompleted {
                                tool_use_id,
                                tool_name,
                                output,
                                is_error,
                                tool_metadata,
                                ..
                            } => {
                                // 本轮全部工具完成时才追加 tool_result 消息→持久化
                                persist = mirror.write().on_tool_completed(
                                    tool_use_id,
                                    output.clone(),
                                    *is_error,
                                );
                                // Tool completion is emitted after dispatch,
                                // so it carries mutations absent from the
                                // preceding AssistantTurnComplete event.
                                last_tool_metadata = Some(tool_metadata.clone());
                                // todo_write 输出同步到待办列表（6.12）
                                if tool_name == "todo_write" && !*is_error {
                                    todos.set(parse_todo_markdown(output));
                                }
                                agent_status.set(AgentStatusView::Thinking);
                            }
                            StreamEvent::AssistantTextDelta { .. } => {
                                agent_status.set(AgentStatusView::Thinking);
                            }
                            StreamEvent::CompactProgress { .. } => {
                                agent_status.set(AgentStatusView::Compacting);
                            }
                            StreamEvent::Compacted { tool_metadata, .. } => {
                                agent_status.set(AgentStatusView::Thinking);
                                // §11.2：Kernel 的压缩上下文仅用于下一次模型请求。
                                // 会话持久化与 compaction archive 均必须保留 host
                                // mirror 中未折叠的完整历史；这里不 save_snapshot，
                                // 也不能以压缩摘要替换 mirror。
                                let _operation = session_operation_gate.lock().await;
                                if event_epoch != session_epoch() {
                                    continue;
                                }
                                let snapshot = mirror.read().snapshot();
                                if let Some(memory) = &memory {
                                    // Compacted 携带的是 tool dispatch 后的当前
                                    // metadata，不能使用 AssistantTurnComplete 时的
                                    // 旧快照。
                                    let checkpoint = checkpoint_from_tool_metadata(tool_metadata);
                                    if let Err(e) = service::save_checkpoint_serialized(
                                        Arc::clone(memory),
                                        snapshot.clone(),
                                        Some(checkpoint),
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            "memory checkpoint (compaction) failed: {e}"
                                        );
                                    }
                                    let extract = memory.clone();
                                    let extraction_token = memory.extraction_token();
                                    spawn(async move {
                                        if let Err(e) = service::extract_durable_serialized(
                                            extract,
                                            extraction_token,
                                            snapshot,
                                            rust_agent::memory::ExtractionReason::Compaction,
                                        )
                                        .await
                                        {
                                            tracing::warn!(
                                                "durable extraction (compaction) failed: {e}"
                                            );
                                        }
                                    });
                                }
                            }
                            StreamEvent::Error { recoverable, .. } => {
                                skill_create.revoke();
                                agent_status.set(if *recoverable {
                                    AgentStatusView::Idle
                                } else {
                                    AgentStatusView::Error
                                });
                            }
                            // 用户 Stop 中断 turn 时 Kernel 只发 Status 事件，
                            // 既不发 Error 也不发 final_turn。一次性授权必须在此
                            // 撤销，否则残留授权可被后续任意 turn 的模型消费
                            // （prompt injection 场景下可创建未授权技能）。
                            StreamEvent::Status { message }
                                if message == QUERY_INTERRUPTED_STATUS =>
                            {
                                skill_create.revoke();
                            }
                            _ => {}
                        }
                        // A Stop click can race a no-tool terminal response:
                        // the Kernel may already be Idle, so it will not
                        // consume the flag or emit QUERY_INTERRUPTED_STATUS.
                        // In that case the terminal turn is the authoritative
                        // acknowledgement; clear both UI and Kernel flags so
                        // the next user message is not falsely interrupted.
                        // A terminal Error event ends the query just the same:
                        // keeping the Kernel flag would leave the next message
                        // immediately interrupted (review 修复).
                        let clear_stale_interrupt = {
                            let mut state = chat.write();
                            let had_pending_interrupt = state.interrupt_pending
                                && (final_turn || matches!(event, StreamEvent::Error { .. }));
                            view_model::apply_stream_event(&mut state, event);
                            if final_turn {
                                view_model::settle_idle(&mut state);
                            }
                            had_pending_interrupt
                        };
                        if clear_stale_interrupt && let Some(flag) = interrupt_sig.read().as_ref() {
                            flag.store(false, Ordering::SeqCst);
                        }
                        if final_turn {
                            skill_create.revoke();
                        }
                        if persist && let Some(store) = &store {
                            let _operation = session_operation_gate.lock().await;
                            // 清空先拿到操作锁时会推进 epoch；该旧事件随后即使
                            // 继续抵达，也不能把旧镜像写入清空后的 current 快照。
                            if event_epoch != session_epoch() {
                                continue;
                            }
                            // 先 clone 快照再 await，不跨 await 持有 Signal 读写守卫
                            let snapshot = mirror.read().snapshot();
                            if let Some(id) = service::save_snapshot(
                                store,
                                &cwd,
                                session_id_sig(),
                                snapshot.clone(),
                                last_usage,
                                last_tool_metadata.clone().unwrap_or_default(),
                            )
                            .await
                            {
                                // §3：当前运行边界首轮生成稳定 session_id 后同步给
                                // MemoryService，保证 checkpoint / digest /
                                // status key 与 SessionStore 同一 session id
                                // （恢复会话已在装配时使用 snapshot 的 id）。
                                if let Some(memory) = &memory {
                                    memory.set_session_id(id.clone());
                                }
                                session_id_sig.set(Some(id));
                            }
                            // final turn：ordered pipeline（§9.2）——checkpoint
                            // 必须有序 await，只有 extraction 可 background。
                            if final_turn && let Some(memory) = &memory {
                                // P2（§10.2）：事件携带的 ToolMetadata 映射
                                // checkpoint；P1 无上游生产时为 None。
                                let checkpoint = last_tool_metadata
                                    .as_ref()
                                    .map(checkpoint_from_tool_metadata);
                                if let Err(e) = service::save_checkpoint_serialized(
                                    Arc::clone(memory),
                                    snapshot.clone(),
                                    checkpoint,
                                )
                                .await
                                {
                                    // checkpoint 失败只 warn/观测，不阻断 Agent
                                    tracing::warn!("memory checkpoint failed: {e}");
                                }
                                let extract = memory.clone();
                                let extraction_token = memory.extraction_token();
                                spawn(async move {
                                    if let Err(e) = service::extract_durable_serialized(
                                        extract,
                                        extraction_token,
                                        snapshot,
                                        rust_agent::memory::ExtractionReason::FinalTurn,
                                    )
                                    .await
                                    {
                                        // extraction 失败也只 warn/观测
                                        tracing::warn!("durable extraction failed: {e}");
                                    }
                                });
                            }
                        }
                        // enter/exit_plan_mode 工具可能改写了模式，回读同步；
                        // agent 发起的退出与用户点击同样推送轻量提示。
                        let new_mode = to_view_mode(engine.mode());
                        let prev_mode = mode();
                        if new_mode != prev_mode {
                            mode.set(new_mode);
                            // 泵协程长寿命：推送时取当前语言而非挂载时快照
                            let tt = i18n.t();
                            let exit_text = if prev_mode == PermissionModeView::Plan {
                                Some(tt.agent_exited_plan_mode)
                            } else if prev_mode == PermissionModeView::FullAuto {
                                Some(tt.agent_exited_full_auto)
                            } else {
                                None
                            };
                            if let Some(text) = exit_text {
                                let id = notice_seq() + 1;
                                notice_seq.set(id);
                                notice.set(Some(NoticeItem {
                                    id,
                                    text: text.to_string(),
                                    kind: NoticeKind::Info,
                                }));
                            }
                        }
                    }
                    // 流关闭（Kernel 退出/异常终止且无末尾 Error 事件）：
                    // 复位忙碌位与状态指示器，避免永久 Thinking 脉冲。
                    // 同时兜底撤销可能残留的 skill_create 一次性授权：
                    // 中断不关闭流、也不触发上方 Error/final_turn 分支，
                    // 此块与中断分支互补，保证授权绝不跨查询存活。
                    skill_create.revoke();
                    {
                        let mut state = chat.write();
                        state.busy = false;
                        state.interrupt_pending = false;
                    }
                    if agent_status() != AgentStatusView::Error {
                        agent_status.set(AgentStatusView::Idle);
                    }
                    // 会话优雅关闭：HNSW 派生缓存落盘（§15：Agent/session
                    // graceful shutdown → save_all）。embeddings 是 SoT，
                    // 保存失败不影响正确性。
                    if let Some(memory) = &memory
                        && let Err(e) = memory.save_all().await
                    {
                        tracing::warn!("memory hnsw cache save failed: {e}");
                    }
                });
            }
        }
    });

    // 提示推送小工具（共用 notice 信号）
    let mut push_notice = move |text: String, kind: NoticeKind| {
        let id = notice_seq() + 1;
        notice_seq.set(id);
        notice.set(Some(NoticeItem { id, text, kind }));
    };

    let on_send = move |text: String| {
        // Stop 的确认尚未到达时保留输入草稿而不发送。旧查询的中断状态
        // 在此时到达会清空 busy；若先开始新查询，UI 会错误显示为 idle。
        if !view_model::can_send(&chat.read()) {
            return false;
        }
        // 未完成初始化时给出提示而非静默丢失输入（初始化含
        // IndexedDB/redb 打开 + 会话恢复，可能耗时或失败）。
        let Some(tx) = event_tx_sig.read().clone() else {
            let hint = init_error
                .read()
                .clone()
                .unwrap_or_else(|| t.agent_initializing.to_string());
            push_notice(hint, NoticeKind::Warning);
            return false;
        };
        // Slash 命令（6.12）：/skill-create 是唯一持久化新技能的入口；
        // /skill <name> 加载技能全文并作为指令发送；/help 展示命令列表。
        // 严格 token 匹配：`/skills x`、`/skillet` 不被误识为命令。
        if let Some(parsed) = parse_skill_create_command(&text) {
            let Ok(request) = parsed else {
                push_notice(
                    t.chat_slash_skill_create_usage.to_string(),
                    NoticeKind::Warning,
                );
                return false;
            };
            let Some(skill_create) = skill_create_sig.read().clone() else {
                push_notice(t.agent_initializing.to_string(), NoticeKind::Warning);
                return false;
            };
            let prompt = skill_create_prompt(&request);
            skill_create.authorize_once();
            if let Some(engine) = engine_sig.read().as_ref() {
                // The command itself is explicit user approval for this
                // tightly scoped, one-shot internal mutation. This
                // session-scoped allowance only suppresses the permission
                // engine's re-prompt; the hard gate stays SkillCreateTool's
                // one-shot authorization CAS, revoked on every completion /
                // error / abort path below. Do not keep one layer without
                // the other.
                engine.allow_for_session("skill_create");
            }
            view_model::push_user(&mut chat.write(), &text);
            scroll_to_latest_request.set(scroll_to_latest_request().wrapping_add(1));
            agent_status.set(AgentStatusView::Thinking);
            let mut tx = tx;
            spawn(async move {
                let sent = tx
                    .send(AgentEvent::UserMessage {
                        content: prompt.clone(),
                        attachments: vec![],
                    })
                    .await;
                if sent.is_ok() {
                    mirror.write().push_user_text(&prompt);
                } else {
                    skill_create.revoke();
                    let mut state = chat.write();
                    view_model::retract_last_user(&mut state, &text);
                    state.busy = false;
                    drop(state);
                    agent_status.set(AgentStatusView::Idle);
                    push_notice(t.agent_send_failed.to_string(), NoticeKind::Warning);
                }
            });
            return true;
        }
        if let Some(rest) = text.strip_prefix("/skill")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            let name = rest.trim().to_string();
            if name.is_empty() {
                push_notice(t.chat_slash_skill.to_string(), NoticeKind::Warning);
                return false;
            }
            // Loading a skill touches storage before it can enqueue the actual
            // prompt.  It therefore needs the same session-boundary check as
            // snapshot/extraction work: a clear while this task is awaiting
            // must discard the old request rather than send it to the new
            // conversation.
            let request_epoch = session_epoch();
            let mut tx = tx;
            let client = client.clone();
            spawn(async move {
                let store = service::open_skill_store(client).await;
                if !session_epoch_is_current(request_epoch, session_epoch()) {
                    return;
                }
                match store {
                    Ok(store) => {
                        // `/skill` is a convenience front-end for the same
                        // gated loader used by the Agent tool.  It must not
                        // bypass a disabled `skill` tool or a skill's current
                        // platform/tool dependencies merely because a user
                        // knows its name.
                        let context = service::current_skill_context();
                        let content = store.load_raw_for_context(&name, &context).await;
                        if !session_epoch_is_current(request_epoch, session_epoch()) {
                            return;
                        }
                        match content {
                            Ok(content) => {
                                let prompt = view_model::skill_prompt(&name, &content);
                                view_model::push_user(&mut chat.write(), &format!("/skill {name}"));
                                scroll_to_latest_request
                                    .set(scroll_to_latest_request().wrapping_add(1));
                                agent_status.set(AgentStatusView::Thinking);
                                // 发送成功后才入镜像：Kernel 已退出时快照不残留
                                // 它从未见过的用户消息（镜像与内核对话保持一致）
                                let sent = tx
                                    .send(AgentEvent::UserMessage {
                                        content: prompt.clone(),
                                        attachments: vec![],
                                    })
                                    .await;
                                if sent.is_ok() {
                                    mirror.write().push_user_text(&prompt);
                                } else {
                                    // 回收可见转写中未送达的消息，与镜像/
                                    // 持久历史保持一致
                                    let mut state = chat.write();
                                    view_model::retract_last_user(
                                        &mut state,
                                        &format!("/skill {name}"),
                                    );
                                    state.busy = false;
                                    drop(state);
                                    agent_status.set(AgentStatusView::Idle);
                                    push_notice(
                                        t.agent_send_failed.to_string(),
                                        NoticeKind::Warning,
                                    );
                                }
                            }
                            Err(err) => push_notice(err.to_string(), NoticeKind::Warning),
                        }
                    }
                    Err(err) => push_notice(err, NoticeKind::Warning),
                }
            });
            return true;
        }
        if text.trim() == "/help" {
            push_notice(
                format!(
                    "{SKILL_CREATE_COMMAND} — {} · /skill — {} · /help — {}",
                    t.chat_slash_skill_create, t.chat_slash_skill, t.chat_slash_help
                ),
                NoticeKind::Info,
            );
            return true;
        }
        view_model::push_user(&mut chat.write(), &text);
        scroll_to_latest_request.set(scroll_to_latest_request().wrapping_add(1));
        agent_status.set(AgentStatusView::Thinking);
        let mut tx = tx;
        spawn(async move {
            // 同 /skill 分支：发送成功后才入镜像，失败提示而非静默丢失
            let sent = tx
                .send(AgentEvent::UserMessage {
                    content: text.clone(),
                    attachments: vec![],
                })
                .await;
            if sent.is_ok() {
                mirror.write().push_user_text(&text);
            } else {
                // 回收可见转写中未送达的消息，与镜像/持久历史保持一致
                let mut state = chat.write();
                view_model::retract_last_user(&mut state, &text);
                state.busy = false;
                drop(state);
                agent_status.set(AgentStatusView::Idle);
                push_notice(t.agent_send_failed.to_string(), NoticeKind::Warning);
            }
        });
        true
    };

    // 真实中断（Phase 7.1）：置位中断标志（Kernel 在模型 turn / 工具批
    // 边界协作式检查，中止本次查询回 Idle）；同时立即复位 UI 忙碌
    // 位与状态指示器（不等待流关闭）。
    let on_interrupt = move |_| {
        if let Some(skill_create) = skill_create_sig.read().as_ref() {
            skill_create.revoke();
        }
        if let Some(flag) = interrupt_sig.read().as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
        // Explicitly deny the dialog currently shown to the user.  This wakes
        // the Kernel immediately; queued dialogs are denied by the bridge
        // pump once they arrive while the cancellation flag remains set.
        if let Some(msg) = pending_perm.write().take() {
            let _ = msg.respond.send(PermissionChoice::Deny);
        }
        // ask_user_question 弹窗同样立即关闭（空答复 = 工具侧按空处理）：
        // 工具批运行期间 Kernel 不检查中断，若不否认，中断须等用户关闭
        // 提问弹窗后才生效（review 修复 S4）。
        if let Some(msg) = pending_question.write().take() {
            let _ = msg.respond.send(String::new());
        }
        view_model::request_interrupt(&mut chat.write());
        agent_status.set(AgentStatusView::Idle);
    };

    let on_clear_conversation = move |_| {
        if clearing_conversation() || chat.read().busy || chat.read().interrupt_pending {
            return;
        }
        let Some(store) = session_store_sig() else {
            clear_error.set(Some(t.agent_clear_unavailable.to_string()));
            return;
        };
        let Some(session_id) = session_id_sig() else {
            clear_error.set(Some(t.agent_clear_unavailable.to_string()));
            return;
        };
        let Some(cwd) = cwd_sig() else {
            clear_error.set(Some(t.agent_clear_unavailable.to_string()));
            return;
        };
        let Some(mut tx) = event_tx_sig() else {
            clear_error.set(Some(t.agent_clear_unavailable.to_string()));
            return;
        };

        let memory = memory_sig();
        let force = force_forget_memories();
        if force && memory.is_none() {
            // 用户已选择不可逆的长期记忆删除，不能在服务不可用时伪装成
            // “删除 0 条”的成功；必须 fail closed。
            clear_error.set(Some(t.agent_clear_memory_unavailable.to_string()));
            return;
        }
        clearing_conversation.set(true);
        clear_error.set(None);
        let operation_gate = Arc::clone(&session_operation_gate);
        spawn(async move {
            // 与 stream pump 的快照保存共用同一把锁。若旧保存已经开始，
            // 本次清空等待它完成后再删除；若本次先取得锁，旧事件会因 epoch
            // 不匹配跳过保存，从而不会复活已删除的历史。
            let _operation = operation_gate.lock().await;
            let outcome = match memory.as_ref() {
                Some(memory) => {
                    match service::clear_current_session_serialized(Arc::clone(memory), force).await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            // Storage errors may contain backend-specific paths or other
                            // diagnostics. Keep those in the local log and present a stable,
                            // localized failure to the user.
                            tracing::warn!(error = %err, "clearing durable session memory failed");
                            clear_error.set(Some(t.agent_clear_failed.to_string()));
                            clearing_conversation.set(false);
                            return;
                        }
                    }
                }
                None => Default::default(),
            };
            if outcome.failed_memories > 0 && !outcome.tombstone_retained {
                clear_error.set(Some(tf(
                    t.agent_clear_partial,
                    &[
                        ("removed", &outcome.removed_memories.to_string()),
                        ("failed", &outcome.failed_memories.to_string()),
                    ],
                )));
                clearing_conversation.set(false);
                return;
            }

            let session_cleanup = match settle_snapshot_cleanup(
                outcome,
                service::clear_session_snapshot_serialized(&store, &cwd, &session_id).await,
            ) {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::warn!(error = %err, "clearing session snapshot failed before commit");
                    clear_error.set(Some(t.agent_clear_failed.to_string()));
                    clearing_conversation.set(false);
                    return;
                }
            };
            if !session_cleanup.cleanup_failures.is_empty() {
                tracing::warn!(
                    cleanup_failures = ?session_cleanup.cleanup_failures,
                    "session clear completed with best-effort cleanup failures"
                );
            }
            session_epoch.set(session_epoch().wrapping_add(1));

            // 仅允许在空闲状态打开确认框；因此该事件会在 Kernel 的下一次
            // Idle 轮询中清空短期上下文，且不会与进行中的模型/工具轮交错。
            // 发送端失败表示 Kernel 已退出；持久化数据已经删除，后续亦不会有
            // 旧上下文被使用，因此仍应收敛 UI 到已清空状态。
            let _kernel_stopped = tx.send(AgentEvent::ClearConversation).await.is_err();

            let new_session_id = generate_session_id(rust_agent::memory::now_ms(), &cwd);
            if let Some(memory) = &memory {
                memory.set_session_id(new_session_id.clone());
            }
            session_id_sig.set(Some(new_session_id));
            mirror.write().clear();
            *chat.write() = ChatViewState::default();
            todos.set(Vec::new());
            agent_status.set(AgentStatusView::Idle);
            force_forget_memories.set(false);
            clear_dialog_open.set(false);
            clearing_conversation.set(false);

            let memory_warning = (outcome.failed_memories > 0).then(|| {
                tf(
                    t.agent_clear_partial_after_history,
                    &[
                        ("removed", &outcome.removed_memories.to_string()),
                        ("failed", &outcome.failed_memories.to_string()),
                    ],
                )
            });
            let (text, kind) = if let Some(warning) = clear_completion_warning(
                memory_warning,
                &session_cleanup.cleanup_failures,
                t.agent_clear_completed_with_cleanup_errors,
            ) {
                (warning, NoticeKind::Warning)
            } else if force {
                (
                    tf(
                        t.agent_clear_success_with_memories,
                        &[("count", &outcome.removed_memories.to_string())],
                    ),
                    NoticeKind::Success,
                )
            } else {
                (t.agent_clear_success.to_string(), NoticeKind::Success)
            };
            let id = notice_seq() + 1;
            notice_seq.set(id);
            notice.set(Some(NoticeItem { id, text, kind }));
        });
    };

    let on_mode_change = move |new_mode: PermissionModeView| {
        let prev = mode();
        if let Some(engine) = engine_sig.read().as_ref() {
            engine.set_mode(to_engine_mode(new_mode));
        }
        mode.set(new_mode);
        // 退出特殊模式（Plan / Full Auto）→ 轻量提示；收紧/进入不提示。
        let exit_text = if prev == PermissionModeView::Plan && new_mode != PermissionModeView::Plan
        {
            Some(t.agent_exited_plan_mode)
        } else if prev == PermissionModeView::FullAuto && new_mode != PermissionModeView::FullAuto {
            Some(t.agent_exited_full_auto)
        } else {
            None
        };
        if let Some(text) = exit_text {
            push_notice(text.to_string(), NoticeKind::Info);
        }
    };

    let permission_view = pending_perm.read().as_ref().map(|msg| msg.view.clone());
    let question_text = pending_question
        .read()
        .as_ref()
        .map(|msg| msg.question.clone());
    let has_chat_notices = init_error.read().is_some()
        || !ready()
        || TOOL_STATE_LOAD_ERROR.read().is_some()
        || PERSIST_ERROR.read().is_some();

    rsx! {
        // 工具状态横幅样式（review 低风险 5）：ToolStateBanner 不再内联
        // `<link>`，由本页面经 ui crate 导出的 asset 句柄统一加载一次
        // tool_panel.css（与 /tools 视图一致）。
        document::Link { rel: "stylesheet", href: ui::TOOL_PANEL_CSS }
        div { class: "ains-agent-chat",
            // 顶部：权限模式切换 + Plan 指示器 + Agent 状态（6.5）
            div { class: "ains-agent-chat__toolbar",
                PermissionModeSwitcher { mode: mode(), on_change: on_mode_change }
                PlanModeIndicator { mode: mode() }
                button {
                    class: "ains-btn ains-btn--secondary",
                    r#type: "button",
                    title: t.agent_clear_conversation,
                    disabled: !ready()
                        || chat.read().busy
                        || chat.read().interrupt_pending
                        || clearing_conversation()
                        || session_store_sig().is_none(),
                    onclick: move |_| {
                        force_forget_memories.set(false);
                        clear_error.set(None);
                        clear_dialog_open.set(true);
                    },
                    Trash2 { width: 15, height: 15 }
                    span { {t.agent_clear_conversation} }
                }
                div { class: "ains-agent-chat__status",
                    AgentStatus { status: agent_status() }
                }
            }

            if has_chat_notices {
                div { class: "ains-agent-chat__notices",
                    if let Some(err) = init_error.read().as_ref() {
                        div { class: "ains-agent-chat__notice ains-agent-chat__notice--error",
                            "{t.agent_init_failed}: {err}"
                        }
                    } else if !ready() {
                        div { class: "ains-agent-chat__notice ains-agent-chat__notice--muted",
                            {t.agent_initializing}
                        }
                    }

                    // 工具状态恢复失败横幅（与 /tools 视图对称，进程级信号
                    // TOOL_STATE_LOAD_ERROR 共享订阅）：fail-open 回退为全部工具
                    // 活跃，需显式告知用户此前停用可能未生效。本进程已有未落盘
                    // 切换时（review 中等问题 3）加载被跳过、内存清单保留，文案
                    // 需区分，避免误导用户以为停用已失效。文案依据写入信号时的
                    // 失败时刻快照，不随会话期间 dirty 变化而漂移。
                    if let Some((err, retained)) = TOOL_STATE_LOAD_ERROR.read().as_ref() {
                        ToolStateBanner {
                            message: format!(
                                "{}: {err}",
                                if *retained {
                                    t.tool_states_load_failed_local
                                } else {
                                    t.tool_states_load_failed
                                },
                            ),
                        }
                    }

                    // 上次切换未落盘横幅（与 /tools 视图对称，进程级信号
                    // PERSIST_ERROR 共享订阅）：落盘任务失败置位、成功清空——会话
                    // 存活期间实时反映 /tools 面板或本会话内切换的落盘结果，替代
                    // 原 AgentBridge 装配时一次性快照。Warning 变体与恢复失败
                    // （Error）区分（review Minor 1）。
                    if let Some(err) = PERSIST_ERROR.read().as_ref() {
                        ToolStateBanner {
                            kind: ToolStateBannerKind::Warning,
                            message: err.clone(),
                        }
                    }
                }
            }

            ChatView { state: chat, scroll_to_latest_request }
            // 待办列表（6.12）：仅在有条目时展示
            if !todos.read().is_empty() {
                div { class: "ains-agent-chat__todos",
                    TodoList { todos }
                }
            }
            ChatInput {
                busy: chat.read().busy && ready(),
                disabled: chat.read().interrupt_pending || !ready() || clearing_conversation(),
                on_send,
                on_interrupt,
                slash_commands: vec![
                    SlashCommandView {
                        name: SKILL_CREATE_COMMAND.into(),
                        description: t.chat_slash_skill_create.to_string(),
                    },
                    SlashCommandView {
                        name: "/skill".into(),
                        description: t.chat_slash_skill.to_string(),
                    },
                    SlashCommandView {
                        name: "/help".into(),
                        description: t.chat_slash_help.to_string(),
                    },
                ],
            }
        }

        // 权限确认弹窗（6.11）：关闭/拒绝均 fail-closed
        if let Some(request) = permission_view {
            PermissionDialog {
                request,
                on_choice: move |choice: PermissionChoice| {
                    if let Some(msg) = pending_perm.write().take() {
                        let _ = msg.respond.send(choice);
                    }
                },
            }
        }

        if clear_dialog_open() {
            Modal {
                title: t.agent_clear_conversation_title.to_string(),
                disable_backdrop: true,
                hide_close: true,
                on_close: move |_| {
                    if !clearing_conversation() {
                        clear_dialog_open.set(false);
                        force_forget_memories.set(false);
                        clear_error.set(None);
                    }
                },
                p { style: "margin:0;color:var(--color-text-secondary);font-size:14px;line-height:1.55;",
                    {t.agent_clear_conversation_message}
                }
                label { style: "display:flex;align-items:flex-start;gap:10px;padding:10px 12px;border:1px solid var(--color-error-border);border-radius:var(--radius-lg);background:var(--color-accent-pink-soft-bg);cursor:pointer;",
                    input {
                        r#type: "checkbox",
                        checked: force_forget_memories(),
                        disabled: clearing_conversation(),
                        onchange: move |event| force_forget_memories.set(event.checked()),
                    }
                    span { style: "display:flex;flex-direction:column;gap:3px;",
                        strong { style: "font-size:13px;color:var(--color-error-text);", {t.agent_clear_force_memories} }
                        span { style: "font-size:12px;color:var(--color-text-secondary);line-height:1.45;",
                            {t.agent_clear_force_memories_hint}
                        }
                    }
                }
                if let Some(error) = clear_error.read().as_ref() {
                    p { class: "ains-form-error", style: "margin:0;", "{error}" }
                }
                div { style: "display:flex;justify-content:flex-end;gap:10px;margin-top:4px;",
                    button {
                        class: "ains-btn ains-btn--secondary",
                        r#type: "button",
                        disabled: clearing_conversation(),
                        onclick: move |_| {
                            clear_dialog_open.set(false);
                            force_forget_memories.set(false);
                            clear_error.set(None);
                        },
                        {t.cancel_label}
                    }
                    button {
                        class: "ains-btn ains-btn--danger",
                        r#type: "button",
                        disabled: clearing_conversation(),
                        onclick: on_clear_conversation,
                        if clearing_conversation() {
                            LoaderCircle { class: "ains-btn__spinner" }
                        }
                        {t.agent_clear_confirm}
                    }
                }
            }
        }

        // ask_user_question 弹窗：关闭返回空答复（工具侧按空处理）
        if let Some(question) = question_text {
            Modal {
                title: t.ask_user_title.to_string(),
                on_close: move |_| {
                    if let Some(msg) = pending_question.write().take() {
                        let _ = msg.respond.send(String::new());
                    }
                    answer_draft.set(String::new());
                },
                div { style: "display:flex;flex-direction:column;gap:12px;",
                    p { style: "margin:0;font-size:14px;white-space:pre-wrap;", "{question}" }
                    textarea {
                        style: "resize:vertical;min-height:64px;padding:12px 16px;border-radius:var(--radius-xl);border:1px solid var(--color-border-default);background:var(--color-input-bg);color:var(--color-text-primary);font-family:var(--font-family);font-size:14px;",
                        placeholder: t.ask_user_placeholder,
                        value: "{answer_draft}",
                        oninput: move |e| answer_draft.set(e.value()),
                    }
                    button {
                        class: "ains-perm__btn ains-perm__btn--allow",
                        style: "align-self:flex-end;",
                        r#type: "button",
                        onclick: move |_| {
                            if let Some(msg) = pending_question.write().take() {
                                let answer = answer_draft.read().clone();
                                let _ = msg.respond.send(answer);
                            }
                            answer_draft.set(String::new());
                        },
                        {t.ask_user_submit}
                    }
                }
            }
        }

        // 轻量非阻塞提示（退出计划/全自动模式）；自动消失，不拦截交互。
        NoticeToast {
            notice,
            on_dismiss: move |id: u64| {
                if notice.read().as_ref().map(|n| n.id) == Some(id) {
                    notice.set(None);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_agent::error::{AgentError, MemoryError};

    #[test]
    fn session_epoch_guard_rejects_skill_request_after_clear() {
        let request_epoch = 41;

        assert!(session_epoch_is_current(request_epoch, request_epoch));
        assert!(
            !session_epoch_is_current(request_epoch, request_epoch.wrapping_add(1)),
            "a skill request that began before clear must not enter the replacement session"
        );
    }

    #[test]
    fn skill_creation_requires_the_explicit_command_and_a_request() {
        assert_eq!(
            parse_skill_create_command("/skill-create release-check # Steps"),
            Some(Ok("release-check # Steps".into()))
        );
        assert_eq!(
            parse_skill_create_command("/skill-create release-check"),
            Some(Ok("release-check".into()))
        );
        assert_eq!(parse_skill_create_command("/skill-create"), Some(Err(())));
        assert_eq!(parse_skill_create_command("/skill-createx name body"), None);
        assert_eq!(parse_skill_create_command("create a skill"), None);
    }

    #[test]
    fn skill_create_prompt_delegates_name_and_workflow_to_the_model() {
        let prompt = skill_create_prompt("创建一个发布检查技能");
        assert!(prompt.contains("Choose the skill name, metadata, and workflow yourself"));
        assert!(prompt.contains("skill_create` exactly once"));
        assert!(prompt.contains("创建一个发布检查技能"));
    }

    #[test]
    fn tombstone_retained_converts_snapshot_cleanup_error_into_warning() {
        let outcome = settle_snapshot_cleanup(
            SessionMemoryClearOutcome {
                tombstone_retained: true,
                ..Default::default()
            },
            Err(AgentError::Memory(MemoryError::Storage(
                "injected snapshot cleanup failure".into(),
            ))),
        )
        .expect("a committed tombstone requires switching away from the old session");

        assert_eq!(outcome.cleanup_failures.len(), 1);
        assert!(outcome.cleanup_failures[0].contains("injected snapshot cleanup failure"));
    }

    #[test]
    fn uncommitted_clear_keeps_snapshot_cleanup_error_retryable() {
        let result = settle_snapshot_cleanup(
            SessionMemoryClearOutcome::default(),
            Err(AgentError::Memory(MemoryError::Storage(
                "injected snapshot cleanup failure".into(),
            ))),
        );

        assert!(result.is_err());
    }

    #[test]
    fn tombstone_cleanup_warning_is_generic() {
        assert!(
            ui::EN
                .agent_clear_completed_with_cleanup_errors
                .contains("some stored data could not be removed")
        );
        assert!(
            !ui::EN
                .agent_clear_completed_with_cleanup_errors
                .contains('{')
        );
    }

    #[test]
    fn completion_warning_never_includes_cleanup_diagnostics() {
        let warning = clear_completion_warning(
            Some("Long-term memory cleanup was partial.".into()),
            &["entry: injected IndexedDB failure at /private/storage".into()],
            ui::EN.agent_clear_completed_with_cleanup_errors,
        )
        .expect("either memory or physical cleanup failure needs a warning");

        assert!(warning.contains("Long-term memory cleanup was partial."));
        assert!(warning.contains("some stored data could not be removed"));
        assert!(!warning.contains("injected IndexedDB failure"));
        assert!(!warning.contains("/private/storage"));
    }
}
