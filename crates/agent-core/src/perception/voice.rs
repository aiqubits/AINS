//! Voice 感知通道（Phase 4.2）：麦克风采集 + Server STT 调用。
//!
//! 平台采集（麦克风）由前端完成；本通道接收已采集的音频字节，经 `ModelClient`
//! 调用服务端 STT 能力转写为文本，产出可注入会话的文本结果。

use crate::error::AgentError;
use crate::model_client::ModelClient;
use crate::perception::PerceptionOutcome;

/// 音频输入的最大字节数（采集护栏）。
pub const MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;

/// Voice 通道（无状态；转写经 `ModelClient::stt`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct VoiceChannel;

impl VoiceChannel {
    pub fn new() -> Self {
        Self
    }

    /// 转写音频为文本（服务端 STT）。
    pub async fn transcribe(
        &self,
        model: &dyn ModelClient,
        audio: &[u8],
    ) -> Result<String, AgentError> {
        validate_audio(audio.len())?;
        model.stt(audio).await
    }

    /// 采集：转写后产出文本感知结果（供落入 ContextStore 成为 user 消息）。
    /// 转写结果为空白时返回空 outcome（`is_empty` 为真，落上下文时被跳过）。
    pub async fn capture(
        &self,
        model: &dyn ModelClient,
        audio: &[u8],
    ) -> Result<PerceptionOutcome, AgentError> {
        let transcript = self.transcribe(model, audio).await?;
        if transcript.trim().is_empty() {
            return Ok(PerceptionOutcome::default());
        }
        Ok(PerceptionOutcome::from_text(transcript).with_source_note("[voice transcript]"))
    }
}

fn validate_audio(len: usize) -> Result<(), AgentError> {
    if len == 0 {
        return Err(AgentError::Model("voice input is empty".into()));
    }
    if len > MAX_AUDIO_BYTES {
        return Err(AgentError::Model(format!(
            "voice input exceeds {MAX_AUDIO_BYTES} bytes"
        )));
    }
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::model_client::{EventStream, ModelRequest, ModelStreamEvent};
    use std::sync::Mutex;

    /// 固定转写结果的 stub ModelClient（ScriptedModelClient 的 stt 恒错，
    /// 故此处用专用 stub 覆盖 Voice 路径）。
    #[derive(Default)]
    struct SttStub {
        reply: String,
        /// 为 true 时 stt 返回错误（模拟服务端 STT 失败）。
        fail: bool,
        received: Mutex<Vec<Vec<u8>>>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl ModelClient for SttStub {
        async fn stream_response(
            &self,
            _request: ModelRequest,
        ) -> Result<EventStream<ModelStreamEvent>, AgentError> {
            Err(AgentError::Model("not used".into()))
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, AgentError> {
            Err(AgentError::Model("not used".into()))
        }
        async fn stt(&self, audio_data: &[u8]) -> Result<String, AgentError> {
            self.received.lock().unwrap().push(audio_data.to_vec());
            if self.fail {
                return Err(AgentError::Model("stt provider failed".into()));
            }
            Ok(self.reply.clone())
        }
        async fn tts(&self, _text: &str) -> Result<Vec<u8>, AgentError> {
            Err(AgentError::Model("not used".into()))
        }
    }

    #[tokio::test]
    async fn transcribe_calls_stt_with_audio_bytes() {
        let stub = SttStub {
            reply: "hello there".into(),
            ..Default::default()
        };
        let text = VoiceChannel::new()
            .transcribe(&stub, &[1, 2, 3])
            .await
            .unwrap();
        assert_eq!(text, "hello there");
        assert_eq!(stub.received.lock().unwrap()[0], vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn capture_wraps_transcript_with_source_note() {
        let stub = SttStub {
            reply: "meeting notes".into(),
            ..Default::default()
        };
        let outcome = VoiceChannel::new().capture(&stub, &[9]).await.unwrap();
        assert_eq!(outcome.text.as_deref(), Some("meeting notes"));
        assert_eq!(outcome.source_note.as_deref(), Some("[voice transcript]"));
    }

    #[tokio::test]
    async fn capture_empty_transcript_yields_empty_outcome() {
        let stub = SttStub {
            reply: "   ".into(),
            ..Default::default()
        };
        let outcome = VoiceChannel::new().capture(&stub, &[9]).await.unwrap();
        assert!(outcome.is_empty());
    }

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let stub = SttStub::default();
        assert!(VoiceChannel::new().transcribe(&stub, &[]).await.is_err());
    }

    #[tokio::test]
    async fn transcribe_audio_size_limit_boundary() {
        // 恰为上限：通过；超限 1 字节：在触达 stt 前拒绝（与 file.rs 的
        // MAX_FILE_BYTES 边界测试同模式）
        let stub = SttStub {
            reply: "ok".into(),
            ..Default::default()
        };
        let at_limit = vec![0u8; MAX_AUDIO_BYTES];
        assert!(
            VoiceChannel::new()
                .transcribe(&stub, &at_limit)
                .await
                .is_ok()
        );
        let over_limit = vec![0u8; MAX_AUDIO_BYTES + 1];
        assert!(
            VoiceChannel::new()
                .transcribe(&stub, &over_limit)
                .await
                .is_err()
        );
        // 超限输入不得透传到 STT 调用
        assert_eq!(stub.received.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn transcribe_propagates_stt_error() {
        // 服务端 STT 失败时，错误应传播而非静默产出空文本
        let stub = SttStub {
            fail: true,
            ..Default::default()
        };
        let err = VoiceChannel::new().transcribe(&stub, &[1, 2, 3]).await;
        assert!(err.is_err());
        // capture 同样传播错误
        assert!(
            VoiceChannel::new()
                .capture(&stub, &[1, 2, 3])
                .await
                .is_err()
        );
    }
}
