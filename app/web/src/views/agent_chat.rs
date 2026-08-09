//! Agent 对话视图（Phase 6.1 + 6.11 接线）。
//!
//! 装配流程：`agent::service::initialize` → 取出 Kernel 后台驱动 →
//! 三条泵协程（stream / 权限确认 / ask_user_question）→ 视图信号。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dioxus::prelude::*;
use futures::{SinkExt, StreamExt};

use agent_core::kernel::{AgentEvent, StreamEvent};
use agent_core::memory::SessionCheckpoint;
use agent_core::model_client::UsageSnapshot;
use agent_core::policy::{PermissionEngine, PermissionMode};
use agent_core::tools::ToolMetadata;
use client_api::Client;
use ui::{
    AgentStatus, AgentStatusView, ChatInput, ChatView, ChatViewState, I18nContext, Modal,
    NoticeItem, NoticeKind, NoticeToast, PERSIST_ERROR, PermissionChoice, PermissionDialog,
    PermissionModeSwitcher, PermissionModeView, PlanModeIndicator, SlashCommandView,
    TOOL_STATE_LOAD_ERROR, TodoItemView, TodoList, ToolStateBanner, ToolStateBannerKind,
    parse_todo_markdown,
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

#[component]
pub fn AgentChat() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    // 宿主在 App 层提供已完成认证配置的 Client（Web 复用 AuthState
    // client；Desktop 从环境变量构造），使本视图可被双端复用。
    let client = use_context::<Client>();

    let mut chat = use_signal(ChatViewState::default);
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
    let mut engine_sig = use_signal(|| None::<Arc<PermissionEngine>>);
    // 中断句柄（Phase 7.1）：停止按钮置位，Kernel 消费后自行清位。
    let mut interrupt_sig = use_signal(|| None::<Arc<AtomicBool>>);
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

    // 装配一次：Kernel + 三条泵协程（本组件卸载时随 scope 取消，
    // event_tx 释放后 Kernel 事件循环优雅退出）。
    use_future(move || {
        let client = client.clone();
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
            engine_sig.set(Some(Arc::clone(&bridge.engine)));
            interrupt_sig.set(Some(Arc::clone(&bridge.interrupt)));
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
                let mut session_id = bridge.session_id.clone();
                // 生产 MemoryService（§8–§11）：final turn / compacted 时
                // ordered checkpoint（await）+ background extraction（spawn）。
                let memory = bridge.memory.clone();
                spawn(async move {
                    let mut last_usage = UsageSnapshot::default();
                    // P2（§10.2）：事件携带的 ToolMetadata → checkpoint 结构化字段
                    let mut last_tool_metadata: Option<ToolMetadata> = None;
                    while let Some(event) = stream_rx.next().await {
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
                                let snapshot = mirror.read().snapshot();
                                if let Some(memory) = &memory {
                                    // Compacted 携带的是 tool dispatch 后的当前
                                    // metadata，不能使用 AssistantTurnComplete 时的
                                    // 旧快照。
                                    let checkpoint = checkpoint_from_tool_metadata(tool_metadata);
                                    if let Err(e) =
                                        memory.save_checkpoint(&snapshot, Some(&checkpoint)).await
                                    {
                                        tracing::warn!(
                                            "memory checkpoint (compaction) failed: {e}"
                                        );
                                    }
                                    let extract = memory.clone();
                                    spawn(async move {
                                        if let Err(e) = service::extract_durable_serialized(
                                            extract,
                                            snapshot,
                                            agent_core::memory::ExtractionReason::Compaction,
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
                                agent_status.set(if *recoverable {
                                    AgentStatusView::Idle
                                } else {
                                    AgentStatusView::Error
                                });
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
                        if persist && let Some(store) = &store {
                            // 先 clone 快照再 await，不跨 await 持有 Signal 读写守卫
                            let snapshot = mirror.read().snapshot();
                            if let Some(id) = service::save_snapshot(
                                store,
                                &cwd,
                                session_id.clone(),
                                snapshot.clone(),
                                last_usage,
                                last_tool_metadata.clone().unwrap_or_default(),
                            )
                            .await
                            {
                                // §3：新会话首轮生成稳定 session_id 后同步给
                                // MemoryService，保证 checkpoint / digest /
                                // status key 与 SessionStore 同一 session id
                                // （恢复会话已在装配时使用 snapshot 的 id）。
                                if let Some(memory) = &memory {
                                    memory.set_session_id(id.clone());
                                }
                                session_id = Some(id);
                            }
                            // final turn：ordered pipeline（§9.2）——checkpoint
                            // 必须有序 await，只有 extraction 可 background。
                            if final_turn && let Some(memory) = &memory {
                                // P2（§10.2）：事件携带的 ToolMetadata 映射
                                // checkpoint；P1 无上游生产时为 None。
                                let checkpoint = last_tool_metadata
                                    .as_ref()
                                    .map(checkpoint_from_tool_metadata);
                                if let Err(e) =
                                    memory.save_checkpoint(&snapshot, checkpoint.as_ref()).await
                                {
                                    // checkpoint 失败只 warn/观测，不阻断 Agent
                                    tracing::warn!("memory checkpoint failed: {e}");
                                }
                                let extract = memory.clone();
                                spawn(async move {
                                    if let Err(e) = service::extract_durable_serialized(
                                        extract,
                                        snapshot,
                                        agent_core::memory::ExtractionReason::FinalTurn,
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
        // Slash 命令（6.12）：/skill <name> 加载技能全文并作为指令发送；
        // /help 展示命令列表；其余按普通文本发送。
        // 严格 token 匹配：`/skills x`、`/skillet` 不被误识为命令。
        if let Some(rest) = text.strip_prefix("/skill")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            let name = rest.trim().to_string();
            if name.is_empty() {
                push_notice(t.chat_slash_skill.to_string(), NoticeKind::Warning);
                return false;
            }
            let mut tx = tx;
            spawn(async move {
                match service::open_skill_store().await {
                    Ok(store) => {
                        match agent_core::skills::SkillLoader::load(&*store, &name).await {
                            Ok(content) => {
                                let prompt = view_model::skill_prompt(&name, &content.body);
                                view_model::push_user(&mut chat.write(), &format!("/skill {name}"));
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
                    "/skill — {} · /help — {}",
                    t.chat_slash_skill, t.chat_slash_help
                ),
                NoticeKind::Info,
            );
            return true;
        }
        view_model::push_user(&mut chat.write(), &text);
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

    rsx! {
        // 工具状态横幅样式（review 低风险 5）：ToolStateBanner 不再内联
        // `<link>`，由本页面经 ui crate 导出的 asset 句柄统一加载一次
        // tool_panel.css（与 /tools 视图一致）。
        document::Link { rel: "stylesheet", href: ui::TOOL_PANEL_CSS }
        div { style: "display:flex;flex-direction:column;height:calc(100vh - 132px);min-height:420px;",
            // 顶部：权限模式切换 + Plan 指示器 + Agent 状态（6.5）
            div { style: "display:flex;align-items:center;gap:12px;padding:0 16px 8px;",
                PermissionModeSwitcher { mode: mode(), on_change: on_mode_change }
                PlanModeIndicator { mode: mode() }
                div { style: "margin-left:auto;",
                    AgentStatus { status: agent_status() }
                }
            }

            if let Some(err) = init_error.read().as_ref() {
                div { style: "padding:16px;color:var(--color-error-text);",
                    "{t.agent_init_failed}: {err}"
                }
            } else if !ready() {
                div { style: "padding:16px;color:var(--color-text-muted);", {t.agent_initializing} }
            }

            // 工具状态恢复失败横幅（与 /tools 视图对称，进程级信号
            // TOOL_STATE_LOAD_ERROR 共享订阅）：fail-open 回退为全部工具
            // 活跃，需显式告知用户此前停用可能未生效。本进程已有未落盘
            // 切换时（review 中等问题 3）加载被跳过、内存清单保留，文案
            // 需区分，避免误导用户以为停用已失效。文案依据写入信号时的
            // 失败时刻快照，不随会话期间 dirty 变化而漂移。
            if let Some((err, retained)) = TOOL_STATE_LOAD_ERROR.read().as_ref() {
                div { style: "margin:0 16px 8px;",
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
            }

            // 上次切换未落盘横幅（与 /tools 视图对称，进程级信号
            // PERSIST_ERROR 共享订阅）：落盘任务失败置位、成功清空——会话
            // 存活期间实时反映 /tools 面板或本会话内切换的落盘结果，替代
            // 原 AgentBridge 装配时一次性快照。Warning 变体与恢复失败
            // （Error）区分（review Minor 1）。
            if let Some(err) = PERSIST_ERROR.read().as_ref() {
                div { style: "margin:0 16px 8px;",
                    ToolStateBanner {
                        kind: ToolStateBannerKind::Warning,
                        message: err.clone(),
                    }
                }
            }

            ChatView { state: chat }
            // 待办列表（6.12）：仅在有条目时展示
            if !todos.read().is_empty() {
                div { style: "padding:0 16px 8px;",
                    TodoList { todos }
                }
            }
            ChatInput {
                busy: chat.read().busy && ready(),
                disabled: chat.read().interrupt_pending || !ready(),
                on_send,
                on_interrupt,
                slash_commands: vec![
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
