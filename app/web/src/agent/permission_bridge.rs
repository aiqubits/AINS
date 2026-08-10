//! 权限确认 / ask_user_question 的 UI 桥接（Phase 6.11）。
//!
//! 设计：rust-agent 的回调 trait 在 Kernel 任务内被调用（native 下要求
//! `Send + Sync`），不能直接持有 Dioxus `Signal`。桥接层以 channel 解耦：
//! 回调侧把请求（含 oneshot 回执发送端）推入 unbounded channel，UI 协程
//! 接收后弹窗，用户点击后经 oneshot 回填。channel/回执任一侧关闭 →
//! Deny / 空答复（fail-closed）。
//!
//! 并发 tool_use 触发的多个确认按 FIFO 逐个弹窗：`confirm` 内部经异步
//! Mutex 串行化（引擎侧已对独占资源顺序执行，这里只保证 UI 一次一窗）。
//!
//! desktop 端经 `#[path]` 引用本文件复用同一实现与测试。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::{mpsc, oneshot};
use futures::lock::Mutex;

use rust_agent::policy::{PermissionPrompt, PermissionReply, PermissionRequest};
use rust_agent::tools::interact::UserInteraction;
use ui::{PermissionChoice, PermissionRequestView};

/// 推给 UI 协程的权限确认请求。
pub struct PermissionPromptMsg {
    pub view: PermissionRequestView,
    pub respond: oneshot::Sender<PermissionChoice>,
}

/// 推给 UI 协程的 ask_user_question 请求。
pub struct InteractionMsg {
    pub question: String,
    pub respond: oneshot::Sender<String>,
}

/// `PermissionPrompt` 的 UI 桥接实现。
pub struct UiPermissionPrompt {
    tx: mpsc::UnboundedSender<PermissionPromptMsg>,
    /// FIFO 弹窗串行化。
    serial: Mutex<()>,
}

impl UiPermissionPrompt {
    /// 返回（回调实现，UI 协程消费的接收端）。
    pub fn channel() -> (Arc<Self>, mpsc::UnboundedReceiver<PermissionPromptMsg>) {
        let (tx, rx) = mpsc::unbounded();
        (
            Arc::new(Self {
                tx,
                serial: Mutex::new(()),
            }),
            rx,
        )
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl PermissionPrompt for UiPermissionPrompt {
    async fn confirm(
        &self,
        request: &PermissionRequest,
        cancel: Option<Arc<AtomicBool>>,
    ) -> PermissionReply {
        // 串行化：上一个弹窗未答复前不投递下一个
        let _guard = self.serial.lock().await;
        // A cancelled query may have been waiting behind another permission
        // dialog.  Do not enqueue a stale second dialog after the first one
        // is dismissed; ToolRuntime also re-checks this flag before execution.
        if cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return PermissionReply::Deny;
        }
        let (respond, reply_rx) = oneshot::channel();
        let msg = PermissionPromptMsg {
            view: to_view(request),
            respond,
        };
        if self.tx.unbounded_send(msg).is_err() {
            // UI 协程已退出 → fail-closed
            return PermissionReply::Deny;
        }
        match reply_rx.await {
            Ok(PermissionChoice::Allow) => PermissionReply::Allow,
            Ok(PermissionChoice::AlwaysAllow) => PermissionReply::AlwaysAllow,
            // 弹窗被销毁 / 显式拒绝 → fail-closed
            Ok(PermissionChoice::Deny) | Err(_) => PermissionReply::Deny,
        }
    }
}

/// `UserInteraction`（ask_user_question）的 UI 桥接实现。
pub struct UiInteraction {
    tx: mpsc::UnboundedSender<InteractionMsg>,
    serial: Mutex<()>,
}

impl UiInteraction {
    pub fn channel() -> (Arc<Self>, mpsc::UnboundedReceiver<InteractionMsg>) {
        let (tx, rx) = mpsc::unbounded();
        (
            Arc::new(Self {
                tx,
                serial: Mutex::new(()),
            }),
            rx,
        )
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl UserInteraction for UiInteraction {
    async fn ask(&self, question: &str) -> String {
        let _guard = self.serial.lock().await;
        let (respond, reply_rx) = oneshot::channel();
        let msg = InteractionMsg {
            question: question.to_string(),
            respond,
        };
        if self.tx.unbounded_send(msg).is_err() {
            return String::new();
        }
        reply_rx.await.unwrap_or_default()
    }
}

/// `PermissionRequest` → 展示模型（参数脱敏）。
///
/// 脱敏复用 [`super::view_model::mask_sensitive`]，与聊天工具卡片同一策略，
/// 保证权限弹窗与转写中敏感字段一致掩码；`command` 字段同样经
/// [`super::view_model::mask_embedded_secrets`] 值级掩码（否则 Arguments
/// 块已掩的秘钥会在 Command 行明文旁路外泄）。
pub fn to_view(request: &PermissionRequest) -> PermissionRequestView {
    let masked = super::view_model::mask_sensitive(request.tool_input.clone());
    PermissionRequestView {
        tool_name: request.tool_name.clone(),
        reason: request.reason.clone(),
        resolved_file_path: request.resolved_file_path.clone(),
        command: request
            .command
            .as_deref()
            .map(super::view_model::mask_embedded_secrets),
        input_preview: serde_json::to_string_pretty(&masked).unwrap_or_else(|_| masked.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_view_carries_context_and_masks_input() {
        let request = PermissionRequest {
            tool_name: "file_write".into(),
            reason: "write requires confirmation".into(),
            tool_input: json!({"file_path": "/tmp/a", "token": "x"}),
            resolved_file_path: Some("/tmp/a".into()),
            command: None,
        };
        let view = to_view(&request);
        assert_eq!(view.tool_name, "file_write");
        assert_eq!(view.resolved_file_path.as_deref(), Some("/tmp/a"));
        assert!(view.input_preview.contains("/tmp/a"));
        assert!(!view.input_preview.contains("\"x\""));
        assert!(view.input_preview.contains("***"));
    }

    #[test]
    fn to_view_masks_embedded_secrets_in_command_field() {
        // Arguments 块已掩的秘钥不得在 Command 行明文旁路（五轮修正）
        let request = PermissionRequest {
            tool_name: "shell_command".into(),
            reason: "shell requires confirmation".into(),
            tool_input: json!({"command": "curl -H 'Authorization: Bearer s3cretT0ken' https://api.x"}),
            resolved_file_path: None,
            command: Some("curl -H 'Authorization: Bearer s3cretT0ken' https://api.x".into()),
        };
        let view = to_view(&request);
        let command = view.command.as_deref().unwrap();
        assert!(!command.contains("s3cretT0ken"), "{command}");
        assert!(command.contains("Bearer ***"));
        assert!(command.contains("curl -H"), "命令结构保留可审阅: {command}");
        assert!(command.contains("https://api.x"));
        // 无 command 的请求仍为 None（不构造空串）
        let mut no_cmd = request;
        no_cmd.command = None;
        assert!(to_view(&no_cmd).command.is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod async_contract {
        use super::super::*;
        use futures::StreamExt;

        fn request() -> PermissionRequest {
            PermissionRequest {
                tool_name: "file_write".into(),
                reason: "confirm".into(),
                tool_input: serde_json::json!({}),
                resolved_file_path: None,
                command: None,
            }
        }

        #[tokio::test]
        async fn confirm_roundtrip_three_replies() {
            for (choice, expected) in [
                (PermissionChoice::Allow, PermissionReply::Allow),
                (PermissionChoice::AlwaysAllow, PermissionReply::AlwaysAllow),
                (PermissionChoice::Deny, PermissionReply::Deny),
            ] {
                let (prompt, mut rx) = UiPermissionPrompt::channel();
                let ui = tokio::spawn(async move {
                    let msg = rx.next().await.expect("prompt message");
                    msg.respond.send(choice).unwrap();
                });
                let reply = prompt.confirm(&request(), None).await;
                assert_eq!(reply, expected);
                ui.await.unwrap();
            }
        }

        #[tokio::test]
        async fn dropped_dialog_is_deny_fail_closed() {
            let (prompt, mut rx) = UiPermissionPrompt::channel();
            let ui = tokio::spawn(async move {
                let msg = rx.next().await.expect("prompt message");
                drop(msg.respond); // UI 弹窗被销毁而未答复
            });
            assert_eq!(
                prompt.confirm(&request(), None).await,
                PermissionReply::Deny
            );
            ui.await.unwrap();
        }

        #[tokio::test]
        async fn closed_channel_is_deny_fail_closed() {
            let (prompt, rx) = UiPermissionPrompt::channel();
            drop(rx); // UI 协程已退出
            assert_eq!(
                prompt.confirm(&request(), None).await,
                PermissionReply::Deny
            );
        }

        #[tokio::test]
        async fn concurrent_confirms_are_serialized_fifo() {
            let (prompt, mut rx) = UiPermissionPrompt::channel();
            let p1 = Arc::clone(&prompt);
            let p2 = Arc::clone(&prompt);
            let t1 = tokio::spawn(async move { p1.confirm(&request(), None).await });
            let t2 = tokio::spawn(async move { p2.confirm(&request(), None).await });

            // 第一个弹窗未答复前，第二个请求不得出现在 channel 中
            let first = rx.next().await.expect("first prompt");
            assert!(
                futures::FutureExt::now_or_never(rx.next()).is_none(),
                "second prompt must wait for the first reply"
            );
            first.respond.send(PermissionChoice::Allow).unwrap();

            let second = rx.next().await.expect("second prompt");
            second.respond.send(PermissionChoice::Deny).unwrap();

            let replies = [t1.await.unwrap(), t2.await.unwrap()];
            assert!(replies.contains(&PermissionReply::Allow));
            assert!(replies.contains(&PermissionReply::Deny));
        }

        #[tokio::test]
        async fn cancelled_prompt_is_denied_without_reaching_the_ui() {
            let (prompt, mut rx) = UiPermissionPrompt::channel();
            let cancel = Arc::new(AtomicBool::new(true));

            assert_eq!(
                prompt.confirm(&request(), Some(cancel)).await,
                PermissionReply::Deny
            );
            assert!(
                futures::FutureExt::now_or_never(rx.next()).is_none(),
                "a cancelled query must not enqueue a stale permission dialog"
            );
        }

        #[tokio::test]
        async fn interaction_roundtrip_and_fail_closed() {
            let (interaction, mut rx) = UiInteraction::channel();
            let ui = tokio::spawn(async move {
                let msg = rx.next().await.expect("question");
                assert_eq!(msg.question, "which one?");
                msg.respond.send("option A".into()).unwrap();
            });
            assert_eq!(interaction.ask("which one?").await, "option A");
            ui.await.unwrap();

            let (interaction, rx) = UiInteraction::channel();
            drop(rx);
            assert_eq!(interaction.ask("anyone?").await, "");
        }
    }
}
