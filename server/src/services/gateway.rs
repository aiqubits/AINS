use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, OsRng, rand_core::RngCore},
};
use anyhow::Context;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chrono::Utc;
use hkdf::Hkdf;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
    sea_query::Expr,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    AutoRouter,
    repositories::channel::{
        ActiveModel, ChannelResponse, Column, Entity, Model, ModelCapability, ProtocolType,
    },
    services::{
        MeteringService, QuotaService,
        dispatch::{self, DispatchAction},
    },
};
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::mpsc::Sender;

/// Maximum number of raw upstream chunks queued per streaming request.
/// A full queue applies backpressure to the upstream HTTP body.
const STREAM_CHANNEL_CAPACITY: usize = 32;

/// Overall deadline for a non-streaming upstream request. Aligned with the
/// reverse proxy's 300s route timeout so long AI operations (STT/TTS, large
/// completions) are not cut off prematurely.
const NON_STREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Per-read idle timeout applied to both the non-streaming and streaming
/// clients. Bounds a provider that accepts the connection but then stops
/// sending data, so a stalled upstream cannot hold request resources open
/// until the overall/route timeout.
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamOutcome {
    Completed,
    Incomplete,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitOutcome {
    Success,
    Failure,
}

fn stream_circuit_outcome(result: &Result<StreamOutcome, GatewayError>) -> Option<CircuitOutcome> {
    match result {
        Ok(StreamOutcome::Completed) => Some(CircuitOutcome::Success),
        Ok(StreamOutcome::Incomplete) => Some(CircuitOutcome::Failure),
        Ok(StreamOutcome::Cancelled) => None,
        // A client-attributable provider 4xx says nothing about channel health.
        Err(e) if e.is_channel_health_failure() => Some(CircuitOutcome::Failure),
        Err(_) => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("No active channel supports this capability")]
    NoChannel,
    #[error("Channel not found")]
    NotFound,
    #[error("Invalid channel input: {0}")]
    InvalidInput(String),
    #[error("Rate limit exceeded: {0}")]
    RateLimited(String),
    /// Client-attributable upstream failure (provider returned 4xx other than
    /// 429). These do NOT reflect channel health and must not trip the circuit
    /// breaker — a malformed client request should never disable a shared
    /// channel for other tenants.
    #[error("Upstream AI provider rejected the request: HTTP {status}")]
    UpstreamClient { status: u16 },
    #[error("Upstream AI provider failed: {0}")]
    Upstream(String),
    #[error("Channel {id} has {usage_count} token usage record(s); cannot delete")]
    HasUsage { id: String, usage_count: u64 },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl GatewayError {
    /// Whether this error reflects a channel-health failure that should count
    /// toward the circuit breaker.
    ///
    /// Only network errors, provider 5xx, provider 429, and invalid provider
    /// responses degrade channel health. Client-attributable 4xx responses
    /// (400/413/422 …) are the caller's fault and are treated as neutral so a
    /// stream of bad requests cannot open a tenant-shared channel.
    fn is_channel_health_failure(&self) -> bool {
        matches!(self, GatewayError::Upstream(_))
    }
}

/// Classify a non-success upstream HTTP status into the appropriate error
/// variant. Client errors (4xx except 429) become [`GatewayError::UpstreamClient`];
/// everything else (429, 5xx) is a channel-health [`GatewayError::Upstream`].
fn upstream_status_error(status: reqwest::StatusCode) -> GatewayError {
    let code = status.as_u16();
    if status.is_client_error() && code != 429 {
        GatewayError::UpstreamClient { status: code }
    } else {
        GatewayError::Upstream(format!("HTTP {code}"))
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct CreateChannelInput {
    pub name: String,
    pub protocol_type: ProtocolType,
    pub models: Vec<String>,
    pub capabilities: Vec<ModelCapability>,
    pub api_key: String,
    pub base_url: String,
    #[serde(default = "default_active")]
    pub is_active: bool,
    #[serde(default = "default_weight")]
    pub weight: i32,
}
fn default_active() -> bool {
    true
}
fn default_weight() -> i32 {
    1
}
#[derive(Debug, serde::Deserialize)]
pub struct UpdateChannelInput {
    pub name: Option<String>,
    pub models: Option<Vec<String>>,
    pub capabilities: Option<Vec<ModelCapability>>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub is_active: Option<bool>,
    pub weight: Option<i32>,
    pub tenant_id: Option<String>,
}

pub struct GatewayService {
    db: Arc<AutoRouter>,
    key: [u8; 32],
    client: reqwest::Client,
    /// Separate client for SSE streaming — uses connect_timeout only (no blanket
    /// request timeout) so long-running SSE connections are not killed after 30s.
    stream_client: reqwest::Client,
    /// Optional quota and circuit-breaker service.
    quota: Option<QuotaService>,
    /// Optional token metering service.
    metering: Option<MeteringService>,
}
impl GatewayService {
    /// Create a new GatewayService.
    ///
    /// `no_proxy` defaults to `false` (use system HTTP proxy), matching the
    /// application config default (`AppConfig.sys_no_proxy`). Override via the
    /// `AINS_SYS_NO_PROXY` environment variable when proxy bypass is needed
    /// (e.g. `AINS_SYS_NO_PROXY=true cargo test` for wiremock-based tests).
    ///
    /// Production code should prefer [`with_quota`] which receives `no_proxy`
    /// from `AppConfig.sys_no_proxy` directly.
    pub fn new(db: Arc<AutoRouter>, secret: &str) -> Self {
        let no_proxy = std::env::var("AINS_SYS_NO_PROXY")
            .ok()
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self::new_with_proxy_flag(db, secret, no_proxy)
    }
    /// Create a GatewayService with an explicit no_proxy flag.
    pub fn new_with_proxy_flag(db: Arc<AutoRouter>, secret: &str, no_proxy: bool) -> Self {
        Self::build_client(db, secret, None, None, no_proxy)
    }
    /// Create a GatewayService with quota management enabled.
    ///
    /// `no_proxy` controls whether the system HTTP proxy is bypassed.
    pub fn with_quota(
        db: Arc<AutoRouter>,
        secret: &str,
        quota: QuotaService,
        metering: Option<MeteringService>,
        no_proxy: bool,
    ) -> Self {
        Self::build_client(db, secret, Some(quota), metering, no_proxy)
    }

    fn build_client(
        db: Arc<AutoRouter>,
        secret: &str,
        quota: Option<QuotaService>,
        metering: Option<MeteringService>,
        no_proxy: bool,
    ) -> Self {
        // Client for non-streaming API calls. Long AI operations (large chat
        // completions, STT transcription, TTS synthesis) can legitimately run
        // well past 30s, so the overall deadline is aligned with the reverse
        // proxy's 300s route timeout. A shorter per-read (idle) timeout still
        // fails fast when a provider accepts the connection but then stops
        // sending data, instead of holding the request open for the full 300s.
        let mut builder = reqwest::Client::builder()
            .timeout(NON_STREAM_REQUEST_TIMEOUT)
            .read_timeout(UPSTREAM_IDLE_TIMEOUT);
        if no_proxy {
            builder = builder.no_proxy();
        }
        let client = builder.build().expect("valid reqwest client");

        // Client for SSE streaming: no blanket request timeout (connections are
        // long-lived), but a per-read idle timeout bounds a provider that opens
        // the stream and then stalls, so a hung upstream cannot pin the request
        // and its resources indefinitely.
        let mut stream_builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(UPSTREAM_IDLE_TIMEOUT)
            .pool_max_idle_per_host(0); // SSE connections are long-lived; don't pool
        if no_proxy {
            stream_builder = stream_builder.no_proxy();
        }
        let stream_client = stream_builder.build().expect("valid reqwest stream client");

        Self {
            db,
            key: derive_key(secret),
            client,
            stream_client,
            quota,
            metering,
        }
    }

    /// List channels with pagination.
    ///
    /// Returns `(items, total_count)` — the handler layer wraps this into
    /// a paginated JSON response.
    pub async fn list_paginated(
        &self,
        tenant_id: Option<&str>,
        page: u64,
        per_page: u64,
    ) -> Result<(Vec<ChannelResponse>, u64), GatewayError> {
        let page = page.clamp(1, 1_000_000);
        let per_page = per_page.clamp(1, 100);

        let mut query = Entity::find().order_by_desc(Column::CreatedAt);
        if let Some(id) = tenant_id {
            query = query.filter(Column::TenantId.eq(id));
        }

        let paginator = query.paginate(&*self.db, per_page);
        let total = paginator.num_items().await.context("count channels")?;

        let items: Vec<ChannelResponse> = paginator
            .fetch_page(page - 1)
            .await
            .context("fetch channels page")?
            .into_iter()
            .map(Into::into)
            .collect();

        Ok((items, total))
    }
    pub async fn create_channel(
        &self,
        input: CreateChannelInput,
        tenant_id: &str,
    ) -> Result<ChannelResponse, GatewayError> {
        self.validate(&input)?;
        let now = Utc::now();
        let model = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id.to_string()),
            name: Set(input.name),
            protocol_type: Set(input.protocol_type.as_str().into()),
            models: Set(serde_json::to_value(input.models).context("serialize models")?),
            capabilities: Set(
                serde_json::to_value(input.capabilities).context("serialize capabilities")?
            ),
            api_key_encrypted: Set(self.encrypt(&input.api_key)?),
            base_url: Set(input.base_url.trim_end_matches('/').to_string()),
            is_active: Set(input.is_active),
            weight: Set(input.weight),
            created_at: Set(now),
            updated_at: Set(now),
        };
        let channel: ChannelResponse = model
            .insert(self.db.write_conn())
            .await
            .context("create channel")?
            .into();
        tracing::info!(channel_id = %channel.id, tenant_id = %channel.tenant_id, name = %channel.name, "Channel created");
        Ok(channel)
    }
    pub async fn update_channel(
        &self,
        id: Uuid,
        input: UpdateChannelInput,
    ) -> Result<ChannelResponse, GatewayError> {
        if input.weight.is_some_and(|v| v < 1) {
            return Err(GatewayError::InvalidInput(
                "weight must be at least 1".into(),
            ));
        }
        let found = Entity::find_by_id(id)
            .one(self.db.write_conn())
            .await
            .context("load channel")?
            .ok_or(GatewayError::NotFound)?;
        let protocol_type = found.protocol_type.clone();
        let mut active: ActiveModel = found.into();
        if let Some(v) = input.name {
            if v.trim().is_empty() {
                return Err(GatewayError::InvalidInput("name is required".into()));
            }
            active.name = Set(v);
        }
        if let Some(v) = input.models {
            active.models = Set(serde_json::to_value(v)
                .map_err(|e| GatewayError::Internal(anyhow::anyhow!("serialize models: {}", e)))?);
        }
        if let Some(v) = input.capabilities {
            if v.is_empty() {
                return Err(GatewayError::InvalidInput(
                    "at least one capability is required".into(),
                ));
            }
            validate_protocol_capabilities(&protocol_type, &v)?;
            active.capabilities = Set(serde_json::to_value(v).map_err(|e| {
                GatewayError::Internal(anyhow::anyhow!("serialize capabilities: {}", e))
            })?);
        }
        if let Some(v) = input.api_key {
            if v.is_empty() {
                return Err(GatewayError::InvalidInput("api_key is required".into()));
            }
            active.api_key_encrypted = Set(self.encrypt(&v)?);
        }
        if let Some(v) = input.base_url {
            let v = v.trim_end_matches('/').to_string();
            if !(v.starts_with("https://") || v.starts_with("http://")) {
                return Err(GatewayError::InvalidInput(
                    "base_url must be an http(s) URL".into(),
                ));
            }
            active.base_url = Set(v);
        }
        if let Some(v) = input.is_active {
            active.is_active = Set(v);
        }
        if let Some(v) = input.weight {
            active.weight = Set(v);
        }
        if let Some(v) = input.tenant_id {
            active.tenant_id = Set(v);
        }
        active.updated_at = Set(Utc::now());
        let result: ChannelResponse = active
            .update(self.db.write_conn())
            .await
            .context("update channel")?
            .into();
        tracing::info!(channel_id = %id, "Channel updated");
        Ok(result)
    }
    /// 物理删除渠道（硬删除）。
    ///
    /// 检查该渠道是否存在关联的 token_usage 记录，若有则拒绝删除
    /// 返回 [`GatewayError::HasUsage`]。清理完历史用量数据后方可删除。
    pub async fn delete_channel(&self, id: Uuid) -> Result<(), GatewayError> {
        // 一致性说明：存在性检查与 token_usage 计数都走 write_conn（主库），
        // 而非默认的读副本路由。token_usage.channel_id 没有外键约束，删除渠道
        // 前的“是否存在用量”校验必须读取到最新写入，否则读副本的复制延迟会让
        // 刚产生的用量记录不可见，从而误删仍被引用的渠道、留下孤儿审计数据。
        let conn = self.db.write_conn();

        // 1. 确认渠道存在
        let _found = Entity::find_by_id(id)
            .one(conn)
            .await
            .context("load channel")?
            .ok_or(GatewayError::NotFound)?;

        // 2. 检查 token_usage 关联记录
        use crate::repositories::token_usage;
        let usage_count = token_usage::Entity::find()
            .filter(token_usage::Column::ChannelId.eq(id))
            .count(conn)
            .await
            .context("count token usage")?;

        if usage_count > 0 {
            return Err(GatewayError::HasUsage {
                id: id.to_string(),
                usage_count,
            });
        }

        // 3. 安全删除
        Entity::delete_by_id(id)
            .exec(conn)
            .await
            .context("delete channel")?;

        tracing::info!(channel_id = %id, "Channel deleted");

        Ok(())
    }
    pub async fn proxy(
        &self,
        tenant_id: &str,
        user_id: &str,
        capability: ModelCapability,
        required_capabilities: &[ModelCapability],
        mut body: Value,
    ) -> Result<Value, GatewayError> {
        let requested_model = dispatch::extract_model(&body);
        let channel = self
            .select_channel(tenant_id, required_capabilities, requested_model.as_deref())
            .await?;
        ensure_request_model(&mut body, &channel)?;

        // Estimate input tokens once from the outbound body — reused for the
        // quota pre-check and, for audio capabilities that omit provider usage,
        // as the synthesized billable input size for metering.
        let input_token_estimate = estimate_tokens(&body);

        // Check quotas before proxying (circuit breaker + rate limits).
        if let Some(ref quota) = self.quota
            && let Err(e) = quota
                .check_all(
                    &channel.id.to_string(),
                    tenant_id,
                    capability.as_str(),
                    input_token_estimate,
                )
                .await
        {
            use crate::services::quota::QuotaError;
            match e {
                QuotaError::CircuitBroken => return Err(GatewayError::NoChannel),
                QuotaError::Unavailable => { /* fail-open: allow request */ }
                other => {
                    return Err(GatewayError::RateLimited(other.to_string()));
                }
            }
        }

        let api_key = self.decrypt(&channel.api_key_encrypted)?;
        let action =
            dispatch::dispatch_proxy(&capability, &channel.protocol_type, &channel.base_url, body)?;

        let model_name = dispatch::extract_model(match &action {
            DispatchAction::JsonPost { body, .. } => body,
            DispatchAction::TtsBinary { body, .. } => body,
            DispatchAction::SttMultipart { model, .. } => {
                let ctx = SttCtx {
                    channel: &channel,
                    tenant_id,
                    user_id,
                    req_type: capability.as_str(),
                    api_key: &api_key,
                };
                return self.send_stt_and_meter(ctx, &action, model).await;
            }
        });

        let mut result = match action {
            DispatchAction::JsonPost { url, body } => {
                if channel.protocol_type == "anthropic" {
                    self.send_anthropic(&url, &api_key, &body).await
                } else {
                    self.send_json(&url, &api_key, &body).await
                }
            }
            DispatchAction::TtsBinary { url, body } => self.send_tts(&url, &api_key, &body).await,
            DispatchAction::SttMultipart { .. } => {
                unreachable!("STT handled above")
            }
        };

        if let Ok(response) = &mut result
            && response.get("model").is_none()
            && let Some(model) = &model_name
            && let Some(object) = response.as_object_mut()
        {
            object.insert("model".into(), Value::String(model.clone()));
        }
        if let Ok(response) = &result
            && let Err(error) = validate_provider_response(&capability, response)
        {
            result = Err(error);
        }

        // Record success/failure for circuit breaker and metering.
        // Client-attributable upstream 4xx responses are neutral: they neither
        // reset nor trip the breaker, so a bad request cannot disable a channel.
        if let Some(ref quota) = self.quota {
            match &result {
                Ok(_) => quota.record_success(&channel.id.to_string()).await,
                Err(e) if e.is_channel_health_failure() => {
                    quota.record_failure(&channel.id.to_string()).await;
                }
                Err(_) => { /* client error: neutral for breaker */ }
            }
        }

        // Record token usage after successful response.
        if let (Ok(response), Some(metering)) = (&result, &self.metering) {
            // user_id comes from the JWT sub claim (snowflake ID, always a valid
            // i64). A parse failure indicates a programming error — log at error
            // level but fall back to 0 so metering (best-effort) doesn't block
            // the proxy response.
            let uid: i64 = user_id.parse().unwrap_or_else(|e| {
                tracing::error!(
                    error = %e,
                    raw_user_id = %user_id,
                    tenant_id = %tenant_id,
                    channel_id = %channel.id,
                    "Failed to parse user_id as i64 — metering will use 0"
                );
                0
            });
            let req_type = capability.as_str().to_string();
            let mdl = model_name.unwrap_or_else(|| "unknown".to_string());
            // Audio responses (TTS/STT) carry no `usage` field, which the
            // metering layer treats as "nothing to record". Synthesize a usage
            // object so the request is still billed; text capabilities always
            // report real usage and are passed through unchanged.
            let synthesized =
                synthesized_usage_for_metering(&capability, input_token_estimate, response);
            let metered = synthesized.as_ref().unwrap_or(response);
            // Best-effort: ignore recording errors (non-fatal).
            let _ = metering
                .record_usage(uid, tenant_id, channel.id, &mdl, &req_type, metered)
                .await;
        }

        result
    }

    /// Proxy a request with streaming (SSE) response.
    ///
    /// Creates a background task that reads streaming chunks from the upstream
    /// provider and sends raw bytes through the returned channel. The caller
    /// (handler layer) is responsible for translating these bytes into the
    /// Responses API SSE format.
    ///
    /// Returns `(receiver, channel_id, selected_model, input_token_estimate)`
    /// so the caller can record metering against the selected channel and
    /// actual model after the stream completes. `input_token_estimate` is the
    /// character-based estimate of the outbound prompt, used as a fallback when
    /// the upstream stream never reports prompt usage (e.g. a stream aborted
    /// before any usage frame, or a provider that omits `stream_options`).
    ///
    /// Quota check is performed upfront before streaming starts.
    /// Token metering is NOT performed by this method — the handler should
    /// accumulate the full response and record metering after stream completion
    /// using the returned `channel_id`.
    pub async fn proxy_stream(
        &self,
        tenant_id: &str,
        _user_id: &str,
        capability: ModelCapability,
        required_capabilities: &[ModelCapability],
        mut body: Value,
    ) -> Result<
        (
            tokio::sync::mpsc::Receiver<Result<Bytes, String>>,
            Uuid,
            String,
            u64,
        ),
        GatewayError,
    > {
        let requested_model = dispatch::extract_model(&body);
        let channel = self
            .select_channel(tenant_id, required_capabilities, requested_model.as_deref())
            .await?;
        ensure_request_model(&mut body, &channel)?;
        if channel.protocol_type == "openai" {
            enable_stream_usage(&mut body)?;
        }
        let selected_model = dispatch::extract_model(&body)
            .ok_or_else(|| GatewayError::InvalidInput("AI request model is required".into()))?;

        // Estimate input tokens once — reused for the quota pre-check and
        // returned to the caller as a metering fallback when the upstream
        // stream never reports prompt usage.
        let input_token_estimate = estimate_tokens(&body);

        // Check quotas before proxying (same as proxy()).
        if let Some(ref quota) = self.quota
            && let Err(e) = quota
                .check_all(
                    &channel.id.to_string(),
                    tenant_id,
                    capability.as_str(),
                    input_token_estimate,
                )
                .await
        {
            use crate::services::quota::QuotaError;
            match e {
                QuotaError::CircuitBroken => return Err(GatewayError::NoChannel),
                QuotaError::Unavailable => { /* fail-open */ }
                other => return Err(GatewayError::RateLimited(other.to_string())),
            }
        }

        let api_key = self.decrypt(&channel.api_key_encrypted)?;
        let action =
            dispatch::dispatch_proxy(&capability, &channel.protocol_type, &channel.base_url, body)?;

        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let client = self.stream_client.clone();
        let channel_id = channel.id;
        let quota = self.quota.clone();

        match action {
            DispatchAction::JsonPost { url, body } => {
                tokio::spawn(async move {
                    let result = if channel.protocol_type == "anthropic" {
                        send_anthropic_stream(&client, &url, &api_key, &body, tx).await
                    } else {
                        send_json_stream(&client, &url, &api_key, &body, tx).await
                    };

                    // Record success/failure for circuit breaker.
                    if let Some(ref q) = quota {
                        match stream_circuit_outcome(&result) {
                            Some(CircuitOutcome::Success) => {
                                q.record_success(&channel_id.to_string()).await
                            }
                            Some(CircuitOutcome::Failure) => {
                                q.record_failure(&channel_id.to_string()).await;
                            }
                            None => {
                                // A downstream disconnect says nothing about the
                                // provider's health; preserve breaker state.
                            }
                        }
                    }
                });
            }
            DispatchAction::TtsBinary { .. } | DispatchAction::SttMultipart { .. } => {
                return Err(GatewayError::InvalidInput(
                    "Streaming is not supported for STT or TTS capabilities".into(),
                ));
            }
        }

        Ok((rx, channel_id, selected_model, input_token_estimate))
    }

    /// Handle STT and record metering (STT has a different flow: multipart upload
    /// without a model field in the dispatch body).
    async fn send_stt_and_meter(
        &self,
        ctx: SttCtx<'_>,
        action: &DispatchAction,
        model: &str,
    ) -> Result<Value, GatewayError> {
        let mut result = match action {
            DispatchAction::SttMultipart {
                url,
                audio_bytes,
                filename,
                form_fields,
                ..
            } => {
                self.send_stt(
                    url,
                    ctx.api_key,
                    audio_bytes.clone(),
                    filename,
                    model,
                    form_fields,
                )
                .await
            }
            _ => unreachable!(),
        };

        if let Ok(response) = &mut result
            && response.get("model").is_none()
            && let Some(object) = response.as_object_mut()
        {
            object.insert("model".into(), Value::String(model.to_string()));
        }
        if let Ok(response) = &result
            && let Err(error) = validate_provider_response(&ModelCapability::Stt, response)
        {
            result = Err(error);
        }

        if let Some(ref quota) = self.quota {
            match &result {
                Ok(_) => quota.record_success(&ctx.channel.id.to_string()).await,
                Err(e) if e.is_channel_health_failure() => {
                    quota.record_failure(&ctx.channel.id.to_string()).await;
                }
                Err(_) => { /* client error: neutral for breaker */ }
            }
        }

        if let (Ok(response), Some(metering)) = (&result, &self.metering) {
            // user_id comes from JWT sub claim (always a valid i64).
            // A parse failure indicates a programming error — log at error
            // level but fall back to 0 so metering (best-effort) doesn't fail.
            let uid: i64 = ctx.user_id.parse().unwrap_or_else(|e| {
                tracing::error!(
                    error = %e,
                    raw_user_id = %ctx.user_id,
                    tenant_id = %ctx.tenant_id,
                    channel_id = %ctx.channel.id,
                    "Failed to parse user_id as i64 — metering will use 0"
                );
                0
            });
            // STT responses carry no `usage` field; synthesize one from the
            // transcription size so the metering layer records the request
            // rather than silently skipping it. Input is audio (no tokens).
            let synthesized = synthesized_usage_for_metering(&ModelCapability::Stt, 0, response);
            let metered = synthesized.as_ref().unwrap_or(response);
            let _ = metering
                .record_usage(
                    uid,
                    ctx.tenant_id,
                    ctx.channel.id,
                    model,
                    ctx.req_type,
                    metered,
                )
                .await;
        }

        result
    }

    /// Standard JSON POST request to an OpenAI-compatible endpoint.
    async fn send_json(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> Result<Value, GatewayError> {
        let response = self
            .client
            .post(url)
            .bearer_auth(api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;
        Self::parse_json_response(response).await
    }

    /// Anthropic /v1/messages request (x-api-key header, different format).
    ///
    /// Translates the request body from Chat Completions format to Anthropic
    /// Messages format before sending, and converts the response back to
    /// Chat Completions format for downstream compatibility.
    async fn send_anthropic(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> Result<Value, GatewayError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(api_key).map_err(|_| {
                GatewayError::Internal(anyhow::anyhow!("invalid anthropic API key header"))
            })?,
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

        // Translate request body from Chat Completions → Anthropic Messages
        let req_body = crate::services::responses::chat_completions_to_anthropic(body);

        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(&req_body)
            .send()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;
        let result = Self::parse_json_response(response).await?;

        // Translate response from Anthropic → Chat Completions
        Ok(crate::services::responses::anthropic_response_to_chat_completions(&result))
    }

    /// STT: multipart POST (audio file -> transcription text).
    async fn send_stt(
        &self,
        url: &str,
        api_key: &str,
        audio_bytes: Vec<u8>,
        filename: &str,
        model: &str,
        form_fields: &[(String, String)],
    ) -> Result<Value, GatewayError> {
        let mime = audio_mime_from_filename(filename);
        let part = reqwest::multipart::Part::bytes(audio_bytes)
            .file_name(filename.to_string())
            .mime_str(mime)
            .map_err(|e| GatewayError::Internal(anyhow::anyhow!("mime: {}", e)))?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", model.to_string());
        for (name, value) in form_fields {
            form = form.text(name.clone(), value.clone());
        }

        let response = self
            .client
            .post(url)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;
        let response_format = form_fields
            .iter()
            .find_map(|(name, value)| (name == "response_format").then_some(value.as_str()))
            .unwrap_or("json");
        if matches!(response_format, "text" | "srt" | "vtt") {
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|error| GatewayError::Upstream(error.to_string()))?;
            if !status.is_success() {
                tracing::warn!(
                    upstream_status = %status.as_u16(),
                    upstream_body = %text,
                    "STT upstream returned error"
                );
                return Err(upstream_status_error(status));
            }
            Ok(serde_json::json!({"text": text}))
        } else {
            Self::parse_json_response(response).await
        }
    }

    /// TTS: JSON POST, binary audio response.
    async fn send_tts(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> Result<Value, GatewayError> {
        let requested_format = body
            .get("response_format")
            .and_then(Value::as_str)
            .unwrap_or("mp3");
        let fallback_content_type = tts_format_content_type(requested_format)?;
        let response = self
            .client
            .post(url)
            .bearer_auth(api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            tracing::warn!(
                upstream_status = %status.as_u16(),
                upstream_body = %body_text,
                "TTS upstream returned error"
            );
            return Err(upstream_status_error(status));
        }

        let content_type = tts_content_type(response.headers(), fallback_content_type)?;

        let bytes = response
            .bytes()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        // Return the binary audio as a base64-encoded JSON response so the
        // client can decode it. For a streaming response, a future upgrade
        // could return raw bytes directly.
        // Audio payloads use RFC 4648 standard base64. URL-safe/no-padding is
        // reserved for the gateway's internal encrypted-key representation.
        let encoded = encode_audio_base64(&bytes);
        Ok(serde_json::json!({
            "audio": encoded,
            "content_type": content_type
        }))
    }

    /// Parse a JSON response from upstream, handling HTTP errors uniformly.
    async fn parse_json_response(response: reqwest::Response) -> Result<Value, GatewayError> {
        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            tracing::warn!(
                upstream_status = %status.as_u16(),
                upstream_body = %body_text,
                "Upstream AI provider returned error"
            );
            return Err(upstream_status_error(status));
        }
        response
            .json::<Value>()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))
    }

    async fn select_channel(
        &self,
        tenant_id: &str,
        required_capabilities: &[ModelCapability],
        model: Option<&str>,
    ) -> Result<Model, GatewayError> {
        let channels = candidate_channel_query(tenant_id, required_capabilities, model)
            .all(&*self.db)
            .await
            .context("load candidate channels")?;
        if channels.is_empty() {
            return Err(GatewayError::NoChannel);
        }
        // Filter out channels whose circuit breaker is open BEFORE weighted
        // selection. Otherwise an open channel could be randomly chosen and the
        // request would fail with NoChannel even though other healthy channels
        // exist. If every candidate is open we surface NoChannel.
        let channels = self.retain_healthy_channels(channels).await;
        weighted_select(&channels)
            .cloned()
            .ok_or(GatewayError::NoChannel)
    }

    /// Drop candidates whose circuit breaker is currently open. When no quota
    /// service is configured (e.g. Redis unavailable / tests) all candidates
    /// are considered healthy.
    async fn retain_healthy_channels(&self, channels: Vec<Model>) -> Vec<Model> {
        let Some(ref quota) = self.quota else {
            return channels;
        };
        let mut healthy = Vec::with_capacity(channels.len());
        for channel in channels {
            let broken = quota
                .is_circuit_broken(&channel.id.to_string())
                .await
                .unwrap_or(false);
            if !broken {
                healthy.push(channel);
            }
        }
        healthy
    }
}

fn tts_content_type(
    headers: &reqwest::header::HeaderMap,
    inferred: &'static str,
) -> Result<&'static str, GatewayError> {
    let Some(value) = headers.get(reqwest::header::CONTENT_TYPE) else {
        return Ok(inferred);
    };
    let value = value
        .to_str()
        .map_err(|_| GatewayError::Upstream("Invalid TTS Content-Type header".into()))?
        .split(';')
        .next()
        .unwrap_or_default()
        .trim();
    match value {
        "audio/mpeg" => Ok("audio/mpeg"),
        "audio/ogg" | "audio/opus" => Ok("audio/ogg"),
        "audio/aac" => Ok("audio/aac"),
        "audio/flac" => Ok("audio/flac"),
        "audio/wav" | "audio/x-wav" => Ok("audio/wav"),
        "audio/pcm" | "audio/L16" => Ok("audio/pcm"),
        "application/octet-stream" | "" => Ok(inferred),
        other => Err(GatewayError::Upstream(format!(
            "Unsupported TTS Content-Type: {other}"
        ))),
    }
}

fn encode_audio_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn tts_format_content_type(requested_format: &str) -> Result<&'static str, GatewayError> {
    let content_type = match requested_format.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pcm" => "audio/pcm",
        other => {
            return Err(GatewayError::InvalidInput(format!(
                "Unsupported TTS response_format: {other}"
            )));
        }
    };
    Ok(content_type)
}

fn candidate_channel_query(
    tenant_id: &str,
    required_capabilities: &[ModelCapability],
    model: Option<&str>,
) -> sea_orm::Select<Entity> {
    let mut query = Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::IsActive.eq(true));
    for capability in required_capabilities {
        query = query.filter(Expr::cust_with_values(
            "capabilities ? CAST($1 AS text)",
            vec![sea_orm::Value::from(capability.as_str())],
        ));
    }
    if required_capabilities.iter().any(|capability| {
        matches!(
            capability,
            ModelCapability::Embedding | ModelCapability::Stt | ModelCapability::Tts
        )
    }) {
        // These upstream APIs are OpenAI-specific. Filtering before weighted
        // selection avoids randomly choosing an unusable legacy channel.
        query = query.filter(Column::ProtocolType.eq("openai"));
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        query = query.filter(Expr::cust_with_values(
            "models ? CAST($1 AS text)",
            vec![sea_orm::Value::from(model)],
        ));
    }
    query
}

fn ensure_request_model(body: &mut Value, channel: &Model) -> Result<(), GatewayError> {
    if body.get("model").and_then(Value::as_str).is_some() {
        return Ok(());
    }
    let model = channel
        .models
        .as_array()
        .and_then(|models| models.first())
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::InvalidInput("Selected channel has no usable model".into()))?;
    body.as_object_mut()
        .ok_or_else(|| GatewayError::InvalidInput("AI request body must be an object".into()))?
        .insert("model".into(), Value::String(model.to_string()));
    Ok(())
}

fn enable_stream_usage(body: &mut Value) -> Result<(), GatewayError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| GatewayError::InvalidInput("AI request body must be an object".into()))?;
    let options = object
        .entry("stream_options")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| GatewayError::InvalidInput("stream_options must be an object".into()))?;
    options.insert("include_usage".into(), Value::Bool(true));
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
//  Streaming proxy helpers
// ═══════════════════════════════════════════════════════════════════

async fn await_stream_or_closed<T>(
    tx: &Sender<Result<Bytes, String>>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        _ = tx.closed() => None,
        output = future => Some(output),
    }
}

const DOWNSTREAM_STREAM_ERROR: &str = "AI provider stream failed";

async fn send_downstream_stream_error(tx: &Sender<Result<Bytes, String>>) {
    // Error notification is best-effort. It must never delay circuit-breaker
    // failure recording when a slow consumer has filled the bounded queue.
    let _ = tx.try_send(Err(DOWNSTREAM_STREAM_ERROR.into()));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiSseEventKind {
    Forward,
    Done,
}

fn decode_sse_event(event: &[u8]) -> Result<&str, &'static str> {
    std::str::from_utf8(event).map_err(|_| "SSE event is not valid UTF-8")
}

fn sse_field_value<'a>(event_block: &'a str, field: &str) -> Option<&'a str> {
    event_block.lines().find_map(|line| {
        let value = line.strip_prefix(field)?.strip_prefix(':')?;
        Some(value.strip_prefix(' ').unwrap_or(value))
    })
}

fn sse_data_payload(event_block: &str) -> Option<String> {
    let mut found = false;
    let mut payload = String::new();
    for line in event_block.lines() {
        let Some(value) = line.strip_prefix("data:") else {
            continue;
        };
        if found {
            payload.push('\n');
        }
        payload.push_str(value.strip_prefix(' ').unwrap_or(value));
        found = true;
    }
    found.then_some(payload)
}

fn inspect_openai_sse_event(event: &[u8]) -> Result<OpenAiSseEventKind, &'static str> {
    let event = decode_sse_event(event)?;
    let event_type = sse_field_value(event, "event").unwrap_or("message");
    if event_type == "error" {
        return Err("OpenAI SSE error event");
    }

    let Some(data) = sse_data_payload(event) else {
        return Ok(OpenAiSseEventKind::Forward);
    };
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(OpenAiSseEventKind::Done);
    }
    if data.is_empty() || matches!(event_type, "ping" | "keepalive") {
        return Ok(OpenAiSseEventKind::Forward);
    }

    let payload: Value =
        serde_json::from_str(data).map_err(|_| "OpenAI SSE event contains invalid JSON")?;
    let is_error = payload.get("error").is_some_and(|error| !error.is_null())
        || payload.get("type").and_then(Value::as_str) == Some("error")
        || payload.get("object").and_then(Value::as_str) == Some("error");
    if is_error {
        Err("OpenAI SSE error payload")
    } else {
        Ok(OpenAiSseEventKind::Forward)
    }
}

async fn forward_validated_openai_sse_event(
    event: Vec<u8>,
    event_data_len: usize,
    tx: &Sender<Result<Bytes, String>>,
) -> Result<Option<StreamOutcome>, GatewayError> {
    let event_kind = match inspect_openai_sse_event(&event[..event_data_len]) {
        Ok(kind) => kind,
        Err(reason) => {
            tracing::error!(reason, "Invalid OpenAI SSE event");
            send_downstream_stream_error(tx).await;
            return Err(GatewayError::Upstream(reason.into()));
        }
    };
    if tx.send(Ok(Bytes::from(event))).await.is_err() {
        return Ok(Some(StreamOutcome::Cancelled));
    }
    if event_kind == OpenAiSseEventKind::Done {
        Ok(Some(StreamOutcome::Completed))
    } else {
        Ok(None)
    }
}

/// Send a streaming JSON POST request (OpenAI-compatible) and forward raw
/// SSE bytes through the channel. A downstream disconnect is distinguished
/// from provider success so it does not reset circuit-breaker state.
async fn send_json_stream(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &Value,
    tx: Sender<Result<Bytes, String>>,
) -> Result<StreamOutcome, GatewayError> {
    let request = client.post(url).bearer_auth(api_key).json(body);
    let Some(response) = await_stream_or_closed(&tx, request.send()).await else {
        return Ok(StreamOutcome::Cancelled);
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, "OpenAI streaming request failed");
            send_downstream_stream_error(&tx).await;
            return Err(GatewayError::Upstream(error.to_string()));
        }
    };

    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            upstream_status = %status.as_u16(),
            "Upstream streaming request returned error"
        );
        send_downstream_stream_error(&tx).await;
        return Err(upstream_status_error(status));
    }

    const MAX_SSE_BUFFER: usize = 1_048_576;
    let mut event_buffer = Vec::new();
    let mut byte_stream = response.bytes_stream();
    loop {
        let Some(chunk) = await_stream_or_closed(&tx, byte_stream.next()).await else {
            return Ok(StreamOutcome::Cancelled);
        };
        let Some(chunk) = chunk else {
            break;
        };
        match chunk {
            Ok(bytes) => {
                if event_buffer.len().saturating_add(bytes.len()) > MAX_SSE_BUFFER {
                    tracing::error!("OpenAI SSE buffer overflow");
                    send_downstream_stream_error(&tx).await;
                    return Err(GatewayError::Upstream("stream buffer overflow".into()));
                }
                event_buffer.extend_from_slice(&bytes);
                while let Some((pos, delimiter_len)) = find_sse_delimiter(&event_buffer) {
                    let remaining = event_buffer.split_off(pos + delimiter_len);
                    let event = std::mem::replace(&mut event_buffer, remaining);
                    if let Some(outcome) =
                        forward_validated_openai_sse_event(event, pos, &tx).await?
                    {
                        return Ok(outcome);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Streaming read error: {}", e);
                send_downstream_stream_error(&tx).await;
                return Err(GatewayError::Upstream(e.to_string()));
            }
        }
    }

    Ok(StreamOutcome::Incomplete)
}

/// Send a streaming request to Anthropic (x-api-key header auth) and forward
/// raw SSE bytes through the channel.
async fn send_anthropic_stream(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &Value,
    tx: Sender<Result<Bytes, String>>,
) -> Result<StreamOutcome, GatewayError> {
    use http::HeaderValue;

    // Translate request from Chat Completions → Anthropic Messages
    let req_body = crate::services::responses::chat_completions_to_anthropic(body);

    // Anthropic requires x-api-key header and anthropic-version
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-api-key",
        HeaderValue::from_str(api_key).map_err(|_| {
            GatewayError::Internal(anyhow::anyhow!("invalid anthropic API key header"))
        })?,
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

    let request = client.post(url).headers(headers).json(&req_body);
    let Some(response) = await_stream_or_closed(&tx, request.send()).await else {
        return Ok(StreamOutcome::Cancelled);
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, "Anthropic streaming request failed");
            send_downstream_stream_error(&tx).await;
            return Err(GatewayError::Upstream(error.to_string()));
        }
    };

    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            upstream_status = %status.as_u16(),
            "Anthropic streaming request returned error"
        );
        send_downstream_stream_error(&tx).await;
        return Err(upstream_status_error(status));
    }

    // Buffer for accumulating SSE events across chunks
    // Safety limit to prevent unbounded growth on malformed upstream data.
    const MAX_SSE_BUFFER: usize = 1_048_576; // 1 MB
    let mut buffer = Vec::new();

    let mut byte_stream = response.bytes_stream();
    loop {
        let Some(chunk) = await_stream_or_closed(&tx, byte_stream.next()).await else {
            return Ok(StreamOutcome::Cancelled);
        };
        let Some(chunk) = chunk else {
            break;
        };
        match chunk {
            Ok(bytes) => {
                // HTTP chunks may split a multi-byte UTF-8 code point. Buffer
                // bytes until a complete SSE event exists, then decode it.
                if buffer.len().saturating_add(bytes.len()) > MAX_SSE_BUFFER {
                    tracing::error!(
                        buffer_bytes = buffer.len().saturating_add(bytes.len()),
                        max = MAX_SSE_BUFFER,
                        "Anthropic SSE buffer overflow — terminating stream"
                    );
                    send_downstream_stream_error(&tx).await;
                    return Err(GatewayError::Upstream("stream buffer overflow".into()));
                }
                buffer.extend_from_slice(&bytes);
                while let Some((pos, delimiter_len)) = find_sse_delimiter(&buffer) {
                    let remaining = buffer.split_off(pos + delimiter_len);
                    let event_block = std::mem::replace(&mut buffer, remaining);
                    let event_bytes = &event_block[..pos];
                    let event_str = match decode_sse_event(event_bytes) {
                        Ok(event) => event,
                        Err(reason) => {
                            tracing::error!(
                                event_bytes = event_bytes.len(),
                                "Anthropic stream produced an invalid UTF-8 SSE event"
                            );
                            send_downstream_stream_error(&tx).await;
                            return Err(GatewayError::Upstream(reason.into()));
                        }
                    };
                    let event_kind = match inspect_anthropic_sse_event(event_str) {
                        Ok(kind) => kind,
                        Err(reason) => {
                            tracing::error!(
                                event_bytes = event_str.len(),
                                reason,
                                "Invalid Anthropic SSE event"
                            );
                            send_downstream_stream_error(&tx).await;
                            return Err(GatewayError::Upstream(reason.into()));
                        }
                    };
                    match event_kind {
                        AnthropicSseEventKind::Error => {
                            tracing::error!("Anthropic SSE error event");
                            send_downstream_stream_error(&tx).await;
                            return Err(GatewayError::Upstream("Anthropic SSE error event".into()));
                        }
                        AnthropicSseEventKind::MessageStop => {
                            if tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await.is_err() {
                                return Ok(StreamOutcome::Cancelled);
                            }
                            return Ok(StreamOutcome::Completed);
                        }
                        AnthropicSseEventKind::Forward => {}
                    }
                    if let Some(converted) = translate_anthropic_sse_event(event_str)
                        && tx.send(Ok(Bytes::from(converted))).await.is_err()
                    {
                        return Ok(StreamOutcome::Cancelled);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Anthropic streaming read error: {}", e);
                send_downstream_stream_error(&tx).await;
                return Err(GatewayError::Upstream(e.to_string()));
            }
        }
    }

    if !buffer.is_empty() {
        tracing::warn!(
            buffered_bytes = buffer.len(),
            "Anthropic stream ended with an incomplete SSE event"
        );
    }

    Ok(StreamOutcome::Incomplete)
}

fn find_sse_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf <= crlf => Some((lf, 2)),
        (Some(_), Some(crlf)) => Some((crlf, 4)),
        (Some(lf), None) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicSseEventKind {
    Forward,
    Error,
    MessageStop,
}

fn anthropic_sse_event_type(event_block: &str) -> &str {
    sse_field_value(event_block, "event").unwrap_or("")
}

fn is_known_anthropic_sse_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "message_start"
            | "content_block_start"
            | "content_block_delta"
            | "content_block_stop"
            | "message_delta"
            | "message_stop"
            | "ping"
            | "error"
    )
}

fn inspect_anthropic_sse_event(event_block: &str) -> Result<AnthropicSseEventKind, &'static str> {
    let event_type = anthropic_sse_event_type(event_block);
    if !is_known_anthropic_sse_event(event_type) {
        return Ok(AnthropicSseEventKind::Forward);
    }

    let data = sse_data_payload(event_block).ok_or("Anthropic SSE event is missing data")?;
    let payload: Value =
        serde_json::from_str(&data).map_err(|_| "Anthropic SSE event contains invalid JSON")?;
    if event_type == "error" || payload.get("type").and_then(Value::as_str) == Some("error") {
        Ok(AnthropicSseEventKind::Error)
    } else if event_type == "message_stop" {
        Ok(AnthropicSseEventKind::MessageStop)
    } else {
        Ok(AnthropicSseEventKind::Forward)
    }
}

/// Translate an Anthropic SSE event block into OpenAI-compatible SSE format.
/// Returns None for events that should be filtered (no content change).
fn translate_anthropic_sse_event(event_block: &str) -> Option<String> {
    // Extract event type from "event: xxx" line
    let event_type = anthropic_sse_event_type(event_block);

    let data_str = sse_data_payload(event_block)?;
    if data_str.is_empty() {
        return None;
    }

    let data: &serde_json::Value = &serde_json::from_str(&data_str).ok()?;

    match event_type {
        "content_block_delta" => {
            use crate::services::responses::extract_anthropic_streaming_delta;
            if let Some(text) = extract_anthropic_streaming_delta(data) {
                let openai_chunk = serde_json::json!({
                    "choices": [{
                        "index": 0,
                        "delta": {"content": text},
                        "finish_reason": null
                    }]
                });
                Some(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&openai_chunk).unwrap_or_default()
                ))
            } else {
                None
            }
        }
        "message_start" => {
            // First event: send role metadata with model name
            let model = data
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            let mut openai_chunk = serde_json::json!({
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": ""},
                    "finish_reason": null
                }]
            });
            if let Some(usage) = data
                .get("message")
                .and_then(|message| message.get("usage"))
                .and_then(openai_usage_from_anthropic)
            {
                openai_chunk["usage"] = usage;
            }
            Some(format!(
                "data: {}\n\n",
                serde_json::to_string(&openai_chunk).unwrap_or_default()
            ))
        }
        "message_delta" => {
            // Final delta with stop_reason
            let stop_reason = data
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|r| r.as_str());
            let finish = match stop_reason {
                Some("end_turn" | "stop_sequence") => Value::String("stop".into()),
                Some("max_tokens") => Value::String("length".into()),
                _ => Value::Null,
            };
            let mut openai_chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish
                }]
            });
            if let Some(usage) = data.get("usage").and_then(openai_usage_from_anthropic) {
                openai_chunk["usage"] = usage;
            }
            Some(format!(
                "data: {}\n\n",
                serde_json::to_string(&openai_chunk).unwrap_or_default()
            ))
        }
        "content_block_start" => {
            // Some text blocks may have initial text in the start event
            if data.get("type").and_then(|t| t.as_str()) == Some("content_block_start")
                && let Some(block) = data.get("content_block")
                && block.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text) = block.get("text").and_then(|t| t.as_str())
                && !text.is_empty()
            {
                let openai_chunk = serde_json::json!({
                    "choices": [{
                        "index": 0,
                        "delta": {"content": text},
                        "finish_reason": null
                    }]
                });
                return Some(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&openai_chunk).unwrap_or_default()
                ));
            }
            None
        }
        // message_stop, ping: no content delta
        _ => None,
    }
}

fn openai_usage_from_anthropic(usage: &Value) -> Option<Value> {
    let mut converted = serde_json::Map::new();
    let input = usage.get("input_tokens").and_then(Value::as_u64);
    let output = usage.get("output_tokens").and_then(Value::as_u64);
    if let Some(input) = input {
        converted.insert("prompt_tokens".into(), Value::from(input));
    }
    if let Some(output) = output {
        converted.insert("completion_tokens".into(), Value::from(output));
    }
    if input.is_some() || output.is_some() {
        converted.insert(
            "total_tokens".into(),
            Value::from(input.unwrap_or(0) + output.unwrap_or(0)),
        );
        Some(Value::Object(converted))
    } else {
        None
    }
}

/// Shared context carrying proxy-request metadata through the STT pipeline,
/// avoiding per-field parameter repetition.
struct SttCtx<'a> {
    channel: &'a Model,
    tenant_id: &'a str,
    user_id: &'a str,
    req_type: &'a str,
    api_key: &'a str,
}

/// Roughly estimate token count from a request body for TPM quota checking.
///
/// Binary media (base64-encoded audio/image/file payloads) is deliberately
/// excluded: a 60s WAV is ~2.5 MB of base64 which, counted as text, would
/// estimate at hundreds of thousands of "tokens" and exhaust the text TPM
/// window before the request ever reaches the provider. Media capacity is
/// governed by request-level (RPM) limits, not text-token accounting.
fn estimate_tokens(body: &Value) -> u64 {
    // Simple estimation: count characters in string values (rough: ~4 chars per token).
    let text_len = estimate_text_length(body);
    (text_len / 4).max(1)
}

/// Object keys whose values carry base64-encoded binary media rather than
/// natural-language text. Their contents must not count toward text TPM.
fn is_binary_media_key(key: &str) -> bool {
    matches!(
        key,
        "file" | "file_data" | "data" | "b64_json" | "input_audio"
    )
}

/// A bare `data:<mime>;base64,…` URI embedded as a plain string value
/// (e.g. an inline image_url). Counting its length as text would grossly
/// over-estimate the token cost of an ordinary image request.
fn is_data_uri(value: &str) -> bool {
    value.starts_with("data:") && value.contains(";base64,")
}

fn estimate_text_length(value: &Value) -> u64 {
    match value {
        Value::String(s) => {
            if is_data_uri(s) {
                0
            } else {
                s.len() as u64
            }
        }
        Value::Array(arr) => arr.iter().map(estimate_text_length).sum(),
        Value::Object(obj) => obj
            .iter()
            .filter(|(key, _)| !is_binary_media_key(key))
            .map(|(_, value)| estimate_text_length(value))
            .sum(),
        _ => 0,
    }
}

/// Synthesize a `usage` object for audio capabilities whose upstream responses
/// carry no token counts, so the request is still metered instead of silently
/// dropped by the metering layer (which skips responses without a `usage`
/// field). Returns `None` when no synthesis is needed — either the provider
/// already reported usage, or the capability is text-based (chat/embedding),
/// whose responses always include real usage.
///
/// - TTS is billed against its input text size (the audio output has no tokens).
/// - STT is billed against the transcription size (the audio input has none).
fn synthesized_usage_for_metering(
    capability: &ModelCapability,
    input_token_estimate: u64,
    response: &Value,
) -> Option<Value> {
    if response.get("usage").is_some() {
        return None;
    }
    let usage = match capability {
        ModelCapability::Tts => serde_json::json!({
            "prompt_tokens": input_token_estimate,
            "completion_tokens": 0,
            "total_tokens": input_token_estimate
        }),
        ModelCapability::Stt => {
            let text_len = response
                .get("text")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0) as u64;
            let completion = text_len / 4;
            serde_json::json!({
                "prompt_tokens": 0,
                "completion_tokens": completion,
                "total_tokens": completion
            })
        }
        _ => return None,
    };
    let mut augmented = response.clone();
    augmented.as_object_mut()?.insert("usage".into(), usage);
    Some(augmented)
}

/// Derive a 32-byte AES key from a secret string via HKDF-SHA256.
fn derive_key(secret: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    let salt = Sha256::digest(b"ains-gateway-key-v1");
    let hk = Hkdf::<Sha256>::new(Some(&salt), secret.as_bytes());
    hk.expand(&[], &mut key)
        .expect("32-byte HKDF expansion should never fail");
    key
}

/// Map a filename extension to its MIME type for STT multipart uploads.
fn audio_mime_from_filename(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "mp3" | "mpeg" | "mpga" => "audio/mpeg",
        "mp4" | "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "webm" => "audio/webm",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn validate_provider_response(
    capability: &ModelCapability,
    response: &Value,
) -> Result<(), GatewayError> {
    let valid = match capability {
        ModelCapability::Chat | ModelCapability::Vision | ModelCapability::WebSearch => {
            crate::services::responses::is_valid_chat_completions_response(response)
        }
        ModelCapability::Embedding => response
            .get("data")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .is_some_and(|items| {
                items.iter().all(|item| {
                    item.get("embedding").is_some_and(|embedding| {
                        embedding.as_array().is_some_and(|values| {
                            !values.is_empty() && values.iter().all(Value::is_number)
                        }) || embedding.as_str().is_some_and(|encoded| {
                            STANDARD.decode(encoded).is_ok_and(|bytes| {
                                !bytes.is_empty()
                                    && bytes.len() % std::mem::size_of::<f32>() == 0
                                    && bytes.chunks_exact(4).all(|chunk| {
                                        f32::from_le_bytes(
                                            chunk.try_into().expect("four-byte chunk"),
                                        )
                                        .is_finite()
                                    })
                            })
                        })
                    })
                })
            }),
        ModelCapability::Stt => response.get("text").and_then(Value::as_str).is_some(),
        ModelCapability::Tts => {
            response
                .get("audio")
                .and_then(Value::as_str)
                .is_some_and(|audio| !audio.is_empty())
                && response
                    .get("content_type")
                    .and_then(Value::as_str)
                    .is_some_and(|content_type| !content_type.is_empty())
        }
    };

    if valid {
        Ok(())
    } else {
        tracing::warn!(
            capability = capability.as_str(),
            "AI provider returned an invalid response"
        );
        Err(GatewayError::Upstream(
            "invalid AI provider response".to_string(),
        ))
    }
}

/// Weighted random selection from candidate channels.
/// Returns `None` if `candidates` is empty or all weights sum to zero.
pub fn weighted_select(candidates: &[Model]) -> Option<&Model> {
    weighted_select_with_rng(candidates, &mut rand::thread_rng())
}

/// Weighted random selection with an explicit random number generator.
/// Useful for deterministic testing.
pub fn weighted_select_with_rng<'a>(
    candidates: &'a [Model],
    rng: &mut impl Rng,
) -> Option<&'a Model> {
    let total: u64 = candidates
        .iter()
        .map(|c| c.weight as u64)
        .fold(0u64, u64::saturating_add);
    if total == 0 {
        return None;
    }
    let mut pick = rng.gen_range(0..total);
    for channel in candidates {
        if pick < channel.weight as u64 {
            return Some(channel);
        }
        pick -= channel.weight as u64;
    }
    // Fallback: the last candidate. Because total > 0 was verified above,
    // candidates cannot be empty, so .last() always returns Some.
    candidates.last()
}

fn validate_protocol_capabilities(
    protocol_type: &str,
    capabilities: &[ModelCapability],
) -> Result<(), GatewayError> {
    if protocol_type == "anthropic"
        && capabilities.iter().any(|capability| {
            matches!(
                capability,
                ModelCapability::Embedding | ModelCapability::Stt | ModelCapability::Tts
            )
        })
    {
        return Err(GatewayError::InvalidInput(
            "Anthropic channels do not support embedding, STT, or TTS capabilities".into(),
        ));
    }
    Ok(())
}

impl GatewayService {
    fn validate(&self, input: &CreateChannelInput) -> Result<(), GatewayError> {
        if input.name.trim().is_empty()
            || input.api_key.is_empty()
            || input.models.is_empty()
            || input.capabilities.is_empty()
            || input.weight < 1
            || !(input.base_url.starts_with("https://") || input.base_url.starts_with("http://"))
        {
            return Err(GatewayError::InvalidInput("name, api_key, models, capabilities, positive weight, and http(s) base_url are required".into()));
        }
        validate_protocol_capabilities(input.protocol_type.as_str(), &input.capabilities)?;
        Ok(())
    }
    fn encrypt(&self, plaintext: &str) -> Result<String, GatewayError> {
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| GatewayError::Internal(anyhow::anyhow!("invalid 32-byte AES key")))?;
        let mut nonce = [0; 12];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .map_err(|_| GatewayError::Internal(anyhow::anyhow!("encrypt channel key")))?;
        Ok(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(ciphertext)
        ))
    }
    fn decrypt(&self, encoded: &str) -> Result<String, GatewayError> {
        let (nonce, ciphertext) = encoded
            .split_once('.')
            .ok_or_else(|| GatewayError::Internal(anyhow::anyhow!("invalid stored channel key")))?;
        let nonce = URL_SAFE_NO_PAD.decode(nonce).context("decode key nonce")?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(ciphertext)
            .context("decode channel key")?;
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| GatewayError::Internal(anyhow::anyhow!("invalid 32-byte AES key")))?;
        let plain = cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| GatewayError::Internal(anyhow::anyhow!("decrypt channel key")))?;
        String::from_utf8(plain)
            .context("stored key is not UTF-8")
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::channel::Model as ChannelModel;
    use chrono::Utc;
    use sea_orm::{DatabaseBackend, DatabaseConnection, QueryTrait};
    use serde_json::Value as Json;
    use uuid::Uuid;

    fn test_channel(name: &str, weight: i32, capabilities: Vec<&str>) -> ChannelModel {
        ChannelModel {
            id: Uuid::new_v4(),
            tenant_id: "default".into(),
            name: name.into(),
            protocol_type: "openai".into(),
            models: Json::Array(vec![Json::String("gpt-4".into())]),
            capabilities: Json::Array(
                capabilities
                    .into_iter()
                    .map(|s| Json::String(s.into()))
                    .collect(),
            ),
            api_key_encrypted: "encrypted".into(),
            base_url: "https://api.test.com".into(),
            is_active: true,
            weight,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn streaming_wait_cancels_when_receiver_is_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            await_stream_or_closed(&tx, std::future::pending::<()>()),
        )
        .await
        .expect("closed stream receiver should cancel the pending operation");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn streaming_wait_returns_ready_output() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert_eq!(await_stream_or_closed(&tx, async { 42 }).await, Some(42));
    }

    #[tokio::test]
    async fn downstream_stream_errors_are_always_generic() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        send_downstream_stream_error(&tx).await;

        assert_eq!(
            rx.recv().await,
            Some(Err(DOWNSTREAM_STREAM_ERROR.to_string()))
        );
    }

    #[tokio::test]
    async fn downstream_stream_error_never_blocks_on_a_full_queue() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(b"queued"))).await.unwrap();

        tokio::time::timeout(Duration::from_millis(10), send_downstream_stream_error(&tx))
            .await
            .expect("circuit-breaker outcome must not wait for queue capacity");
    }

    #[test]
    fn cancelled_stream_does_not_change_circuit_breaker_state() {
        assert_eq!(stream_circuit_outcome(&Ok(StreamOutcome::Cancelled)), None);
        assert_eq!(
            stream_circuit_outcome(&Ok(StreamOutcome::Completed)),
            Some(CircuitOutcome::Success)
        );
        assert_eq!(
            stream_circuit_outcome(&Ok(StreamOutcome::Incomplete)),
            Some(CircuitOutcome::Failure)
        );
        assert_eq!(
            stream_circuit_outcome(&Err(GatewayError::Upstream("failed".into()))),
            Some(CircuitOutcome::Failure)
        );
    }

    #[test]
    fn byte_buffer_preserves_unicode_split_across_http_chunks() {
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好🙂\"}}\n\n";
        let unicode_start = event.find('你').unwrap();
        let split = unicode_start + 1;
        let mut buffer = Vec::new();

        buffer.extend_from_slice(&event.as_bytes()[..split]);
        assert!(find_sse_delimiter(&buffer).is_none());
        buffer.extend_from_slice(&event.as_bytes()[split..]);

        let (pos, delimiter_len) = find_sse_delimiter(&buffer).unwrap();
        let event_bytes = &buffer[..pos];
        let decoded = std::str::from_utf8(event_bytes).unwrap();
        let translated = translate_anthropic_sse_event(decoded).unwrap();
        assert!(translated.contains("你好🙂"));
        assert_eq!(pos + delimiter_len, buffer.len());
    }

    #[test]
    fn recognizes_only_explicit_openai_done_events() {
        assert_eq!(
            inspect_openai_sse_event(b"data: [DONE]").unwrap(),
            OpenAiSseEventKind::Done
        );
        assert_eq!(
            inspect_openai_sse_event(b"event: done\r\ndata:[DONE]").unwrap(),
            OpenAiSseEventKind::Done
        );
        assert_eq!(
            inspect_openai_sse_event(br#"data: {"choices":[{"delta":{"content":"[DONE]"}}]}"#)
                .unwrap(),
            OpenAiSseEventKind::Forward
        );
    }

    #[test]
    fn rejects_openai_in_band_error_events() {
        let events: [&[u8]; 4] = [
            b"event: error\ndata: {\"message\":\"failed\"}",
            b"data: {\"error\":{\"message\":\"failed\",\"upstream_secret\":\"hidden\"}}",
            b"data: {\"type\":\"error\",\"message\":\"failed\"}",
            b"data: {\"object\":\"error\",\"message\":\"failed\"}",
        ];
        for event in events {
            assert!(inspect_openai_sse_event(event).is_err());
        }
    }

    #[tokio::test]
    async fn openai_in_band_error_is_generic_and_fails_circuit_outcome() {
        let event = b"data: {\"error\":{\"message\":\"sensitive upstream detail\"}}\n\n".to_vec();
        let data_len = event.len() - 2;
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);

        let result = forward_validated_openai_sse_event(event, data_len, &tx).await;
        let stream_result = result.map(|outcome| outcome.unwrap_or(StreamOutcome::Incomplete));

        assert_eq!(
            stream_circuit_outcome(&stream_result),
            Some(CircuitOutcome::Failure)
        );
        let downstream = rx.recv().await.unwrap().unwrap_err();
        assert_eq!(downstream, DOWNSTREAM_STREAM_ERROR);
        assert!(!downstream.contains("sensitive upstream detail"));
        assert!(
            rx.try_recv().is_err(),
            "raw provider error must not be forwarded"
        );
    }

    #[test]
    fn openai_null_error_field_is_not_a_failure() {
        assert_eq!(
            inspect_openai_sse_event(
                br#"data: {"error":null,"choices":[{"delta":{"content":"ok"}}]}"#
            )
            .unwrap(),
            OpenAiSseEventKind::Forward
        );
    }

    #[test]
    fn rejects_invalid_openai_sse_utf8_and_json() {
        assert!(inspect_openai_sse_event(b"data: {not-json}").is_err());
        assert!(inspect_openai_sse_event(b"data: \xff").is_err());
    }

    #[test]
    fn recognizes_anthropic_error_event_type() {
        assert_eq!(
            anthropic_sse_event_type(
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"failed\"}}"
            ),
            "error"
        );
        assert_eq!(
            inspect_anthropic_sse_event(
                "event:error\ndata:{\"type\":\"error\",\"error\":{\"message\":\"failed\"}}"
            )
            .unwrap(),
            AnthropicSseEventKind::Error
        );
    }

    #[test]
    fn rejects_invalid_known_anthropic_events() {
        assert!(
            inspect_anthropic_sse_event("event: content_block_delta\ndata: {not-json}").is_err()
        );
        assert!(inspect_anthropic_sse_event("event: message_stop").is_err());
        assert!(decode_sse_event(b"event: message_stop\ndata: \xff").is_err());
    }

    #[test]
    fn valid_anthropic_message_stop_is_terminal() {
        assert_eq!(
            inspect_anthropic_sse_event("event: message_stop\ndata: {\"type\":\"message_stop\"}")
                .unwrap(),
            AnthropicSseEventKind::MessageStop
        );
    }

    #[test]
    fn candidate_query_requires_every_capability_and_requested_model() {
        let query = candidate_channel_query(
            "tenant-a",
            &[ModelCapability::Chat, ModelCapability::Vision],
            Some("gpt-4o"),
        );
        let statement = query.build(DatabaseBackend::Postgres);
        let sql = statement.sql;

        assert_eq!(sql.matches("capabilities ? CAST(").count(), 2);
        assert_eq!(sql.matches("models ? CAST(").count(), 1);
        assert!(sql.contains("tenant_id"));
        assert!(sql.contains("is_active"));
    }

    #[test]
    fn direct_capability_query_excludes_non_openai_channels() {
        for capability in [
            ModelCapability::Embedding,
            ModelCapability::Stt,
            ModelCapability::Tts,
        ] {
            let statement = candidate_channel_query("tenant-a", &[capability], None)
                .build(DatabaseBackend::Postgres);
            assert!(
                statement.sql.contains("protocol_type"),
                "direct capability query must constrain the upstream protocol: {}",
                statement.sql
            );
        }
    }

    #[test]
    fn anthropic_channel_rejects_openai_only_capabilities() {
        for capability in [
            ModelCapability::Embedding,
            ModelCapability::Stt,
            ModelCapability::Tts,
        ] {
            assert!(validate_protocol_capabilities("anthropic", &[capability]).is_err());
        }
        assert!(
            validate_protocol_capabilities(
                "anthropic",
                &[ModelCapability::Chat, ModelCapability::Vision],
            )
            .is_ok()
        );
    }

    #[test]
    fn missing_request_model_uses_first_channel_model() {
        let channel = test_channel("chat", 1, vec!["chat"]);
        let mut body = serde_json::json!({"messages": []});

        ensure_request_model(&mut body, &channel).unwrap();

        assert_eq!(body["model"], "gpt-4");
    }

    #[test]
    fn openai_streaming_requests_include_usage_summary() {
        let mut body = serde_json::json!({"model": "gpt-4", "stream": true});

        enable_stream_usage(&mut body).unwrap();

        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn tts_format_maps_to_expected_content_type() {
        let cases = [
            ("mp3", "audio/mpeg"),
            ("opus", "audio/ogg"),
            ("aac", "audio/aac"),
            ("flac", "audio/flac"),
            ("wav", "audio/wav"),
            ("pcm", "audio/pcm"),
        ];
        for (format, expected) in cases {
            assert_eq!(tts_format_content_type(format).unwrap(), expected);
        }
        assert!(tts_format_content_type("avi").is_err());
    }

    #[test]
    fn stt_audio_extensions_map_to_supported_mime_types() {
        let cases = [
            ("audio.wav", "audio/wav"),
            ("audio.mp3", "audio/mpeg"),
            ("audio.mpeg", "audio/mpeg"),
            ("audio.mpga", "audio/mpeg"),
            ("audio.mp4", "audio/mp4"),
            ("audio.m4a", "audio/mp4"),
            ("audio.webm", "audio/webm"),
        ];
        for (filename, expected) in cases {
            assert_eq!(audio_mime_from_filename(filename), expected);
        }
        assert_eq!(
            audio_mime_from_filename("audio.unknown"),
            "application/octet-stream"
        );
    }

    #[test]
    fn malformed_provider_success_payloads_are_rejected() {
        assert!(
            validate_provider_response(&ModelCapability::Chat, &serde_json::json!({})).is_err()
        );
        assert!(
            validate_provider_response(
                &ModelCapability::Embedding,
                &serde_json::json!({"data": []}),
            )
            .is_err()
        );
        assert!(validate_provider_response(&ModelCapability::Stt, &serde_json::json!({})).is_err());
        assert!(
            validate_provider_response(
                &ModelCapability::Tts,
                &serde_json::json!({"audio": "YWJj"}),
            )
            .is_err()
        );

        assert!(
            validate_provider_response(
                &ModelCapability::Embedding,
                &serde_json::json!({"data": [{"embedding": "AACAPw=="}]}),
            )
            .is_ok()
        );
        assert!(
            validate_provider_response(&ModelCapability::Stt, &serde_json::json!({"text": ""}),)
                .is_ok()
        );
        assert!(
            validate_provider_response(
                &ModelCapability::Chat,
                &serde_json::json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": null},
                        "finish_reason": "content_filter"
                    }]
                }),
            )
            .is_ok()
        );
        assert!(
            validate_provider_response(
                &ModelCapability::Chat,
                &serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "refusal": "I cannot help with that."
                        },
                        "finish_reason": "stop"
                    }]
                }),
            )
            .is_ok()
        );
    }

    #[test]
    fn tts_audio_uses_standard_padded_base64() {
        assert_eq!(encode_audio_base64(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn tts_content_type_prefers_valid_upstream_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("audio/x-wav; charset=binary"),
        );
        assert_eq!(
            tts_content_type(&headers, "audio/mpeg").unwrap(),
            "audio/wav"
        );

        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/octet-stream"),
        );
        assert_eq!(
            tts_content_type(&headers, "audio/flac").unwrap(),
            "audio/flac"
        );
    }

    fn service() -> GatewayService {
        GatewayService::new(
            AutoRouter::single(DatabaseConnection::Disconnected),
            "test gateway encryption key",
        )
    }

    #[test]
    fn channel_key_is_encrypted_and_round_trips() {
        let service = service();
        let encrypted = service.encrypt("sk-secret").unwrap();
        assert_ne!(encrypted, "sk-secret");
        assert_eq!(service.decrypt(&encrypted).unwrap(), "sk-secret");
    }

    #[test]
    fn different_encryption_keys_produce_different_ciphertexts() {
        let s1 = GatewayService::new(
            AutoRouter::single(DatabaseConnection::Disconnected),
            "key-one",
        );
        let s2 = GatewayService::new(
            AutoRouter::single(DatabaseConnection::Disconnected),
            "key-two",
        );
        let c1 = s1.encrypt("same-plaintext").unwrap();
        let c2 = s2.encrypt("same-plaintext").unwrap();
        // Different keys produce different ciphertexts (even with random nonces)
        assert_ne!(c1, c2);
        // Each key can decrypt its own ciphertext
        assert_eq!(s1.decrypt(&c1).unwrap(), "same-plaintext");
        assert_eq!(s2.decrypt(&c2).unwrap(), "same-plaintext");
    }

    #[test]
    fn capability_wire_names_match_gateway_contract() {
        assert_eq!(ModelCapability::Embedding.as_str(), "embedding");
        assert_eq!(
            serde_json::to_string(&ModelCapability::Stt).unwrap(),
            "\"stt\""
        );
        assert_eq!(ModelCapability::WebSearch.as_str(), "websearch");
        assert_eq!(
            serde_json::to_string(&ModelCapability::WebSearch).unwrap(),
            "\"websearch\""
        );
        assert_eq!(ProtocolType::Openai.as_str(), "openai");
    }

    #[test]
    fn weighted_select_empty_returns_none() {
        assert!(weighted_select(&[]).is_none());
    }

    #[test]
    fn weighted_select_zero_total_returns_none() {
        let channels = vec![test_channel("a", 0, vec!["chat"])];
        assert!(weighted_select(&channels).is_none());
    }

    #[test]
    fn weighted_select_single_channel_returns_it() {
        let channels = vec![test_channel("only", 5, vec!["chat"])];
        let selected = weighted_select(&channels);
        assert_eq!(selected.map(|c| c.name.as_str()), Some("only"));
    }

    #[test]
    fn weighted_select_obeys_distribution_uniform_weights() {
        let channels = vec![
            test_channel("a", 1, vec!["chat"]),
            test_channel("b", 1, vec!["chat"]),
        ];
        // Run many trials; both channels should be selected at least once
        let mut seen_a = false;
        let mut seen_b = false;
        for _ in 0..100 {
            let sel = weighted_select(&channels).unwrap();
            if sel.name == "a" {
                seen_a = true;
            }
            if sel.name == "b" {
                seen_b = true;
            }
        }
        assert!(
            seen_a && seen_b,
            "both channels should be selected over 100 trials"
        );
    }

    #[test]
    fn weighted_select_favors_higher_weight() {
        let channels = vec![
            test_channel("heavy", 10, vec!["chat"]),
            test_channel("light", 1, vec!["chat"]),
        ];
        let mut heavy_count = 0;
        let trials = 200;
        for _ in 0..trials {
            let sel = weighted_select(&channels).unwrap();
            if sel.name == "heavy" {
                heavy_count += 1;
            }
        }
        // Heavy (weight 10/11) should be selected more often than light (1/11).
        // With 200 trials, heavy should be > 100 (>50% expected ~90.9%).
        assert!(
            heavy_count > 100,
            "heavy channel should be selected more often (got {}/{})",
            heavy_count,
            trials
        );
    }

    #[test]
    fn weighted_select_with_rng_known_pick_chooses_correct_channel() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;

        let channels = vec![
            test_channel("first", 1, vec!["chat"]),
            test_channel("second", 5, vec!["chat"]),
        ];
        // With a seeded StdRng, weighted_select_with_rng is deterministic.
        // We just verify that the function completes without error and
        // returns Some — the exact result depends on the RNG impl's output.
        let mut rng = StdRng::seed_from_u64(42);
        let result = weighted_select_with_rng(&channels, &mut rng);
        assert!(
            result.is_some(),
            "seeded RNG must produce a valid selection"
        );

        // Two consecutive calls with the same seed produce the same result.
        let mut rng_a = StdRng::seed_from_u64(99);
        let mut rng_b = StdRng::seed_from_u64(99);
        assert_eq!(
            weighted_select_with_rng(&channels, &mut rng_a).map(|c| c.name.as_str()),
            weighted_select_with_rng(&channels, &mut rng_b).map(|c| c.name.as_str()),
            "same seed must produce same selection"
        );
    }

    #[test]
    fn weighted_select_all_zero_weights_returns_none() {
        let channels = vec![
            test_channel("a", 0, vec!["chat"]),
            test_channel("b", 0, vec!["chat"]),
        ];
        assert!(
            weighted_select(&channels).is_none(),
            "all-zero weights should return None"
        );
    }

    #[test]
    fn encrypt_empty_string_round_trips() {
        let service = service();
        let encrypted = service.encrypt("").unwrap();
        assert!(
            !encrypted.is_empty(),
            "empty plaintext should produce ciphertext"
        );
        assert_eq!(service.decrypt(&encrypted).unwrap(), "");
    }

    #[test]
    fn decrypt_invalid_format_returns_error() {
        let service = service();
        let result = service.decrypt("not-enough-dots");
        assert!(result.is_err(), "no-dot format should fail");

        let result = service.decrypt("too.many.dots");
        assert!(result.is_err(), "multiple dots format should fail");
    }

    #[test]
    fn decrypt_wrong_key_fails() {
        let s1 = GatewayService::new(
            AutoRouter::single(DatabaseConnection::Disconnected),
            "correct key for encryption",
        );
        let s2 = GatewayService::new(
            AutoRouter::single(DatabaseConnection::Disconnected),
            "wrong key for decryption",
        );
        let encrypted = s1.encrypt("sk-secret").unwrap();
        let result = s2.decrypt(&encrypted);
        assert!(result.is_err(), "decrypting with wrong key should fail");
    }

    #[test]
    fn encrypt_empty_secret_derives_key_via_hkdf_and_round_trips() {
        // An empty secret string must still produce a valid derived key
        // via HKDF-SHA256 and support encrypt/decrypt round-trip.
        let service = GatewayService::new(AutoRouter::single(DatabaseConnection::Disconnected), "");
        let encrypted = service.encrypt("sk-test-key").unwrap();
        assert!(
            !encrypted.is_empty(),
            "should produce ciphertext even with empty secret"
        );
        assert_ne!(
            encrypted, "sk-test-key",
            "ciphertext must differ from plaintext"
        );
        assert_eq!(
            service.decrypt(&encrypted).unwrap(),
            "sk-test-key",
            "round-trip with empty-secret derived key should succeed"
        );
    }

    #[test]
    fn encrypt_empty_string_with_empty_secret() {
        // Edge case: both secret and plaintext are empty.
        let service = GatewayService::new(AutoRouter::single(DatabaseConnection::Disconnected), "");
        let encrypted = service.encrypt("").unwrap();
        assert!(
            !encrypted.is_empty(),
            "empty plaintext should still produce ciphertext"
        );
        assert_eq!(service.decrypt(&encrypted).unwrap(), "");
    }

    // ═══════════════════════════════════════════════════════════════
    //  Anthropic SSE event translation tests
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn translate_anthropic_sse_content_block_delta() {
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(
            result.is_some(),
            "content_block_delta should produce a delta event"
        );
        let sse = result.unwrap();
        assert!(sse.contains("data: {"), "should contain JSON data");
        assert!(
            sse.contains("Hello") || sse.contains("delta"),
            "should contain the text delta"
        );
        assert!(sse.ends_with("\n\n"), "should end with double newline");
    }

    #[test]
    fn translate_anthropic_sse_message_start() {
        let event = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-opus-20240229\",\"usage\":{\"input_tokens\":10}}}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(result.is_some(), "message_start should produce an event");
        let sse = result.unwrap();
        assert!(
            sse.contains("role\":\"assistant\"") || sse.contains("\\\"role\\\":\\\"assistant\\\""),
            "should set role to assistant"
        );
        assert!(
            sse.contains("\"prompt_tokens\":10"),
            "message_start usage should preserve input tokens"
        );
    }

    #[test]
    fn translate_anthropic_sse_message_delta() {
        let event = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(
            result.is_some(),
            "message_delta should produce a finish event"
        );
        let sse = result.unwrap();
        // end_turn should be mapped to "stop" finish_reason
        assert!(sse.contains("stop"), "end_turn should be mapped to stop");
        assert!(sse.contains("\"prompt_tokens\":10"));
        assert!(sse.contains("\"completion_tokens\":20"));
    }

    #[test]
    fn translate_anthropic_sse_message_stop_returns_none() {
        let event = "event: message_stop\ndata: {\"type\":\"message_stop\"}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(result.is_none(), "message_stop should be filtered out");
    }

    #[test]
    fn translate_anthropic_sse_ping_returns_none() {
        let event = "event: ping\ndata: {\"type\":\"ping\"}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(result.is_none(), "ping events should be filtered out");
    }

    #[test]
    fn translate_anthropic_sse_empty_data_returns_none() {
        // Malformed: event line but no data line
        let event = "event: content_block_delta\n";
        let result = super::translate_anthropic_sse_event(event);
        assert!(result.is_none(), "missing data line should return None");
    }

    #[test]
    fn translate_anthropic_sse_malformed_json_returns_none() {
        let event = "event: content_block_delta\ndata: {invalid json}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(result.is_none(), "malformed JSON data should return None");
    }

    #[test]
    fn translate_anthropic_sse_content_block_start_with_text() {
        let event = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Initial text\"}}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(
            result.is_some(),
            "content_block_start with text should produce a delta"
        );
        let sse = result.unwrap();
        assert!(
            sse.contains("Initial text"),
            "should include initial block text"
        );
    }

    #[test]
    fn translate_anthropic_sse_content_block_start_empty_text_returns_none() {
        let event = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(
            result.is_none(),
            "empty text in content_block_start should be filtered"
        );
    }

    #[test]
    fn translate_anthropic_sse_no_event_type_line() {
        // Lines without "event:" prefix should default to empty string and be filtered
        let data = "data: {\"type\":\"ping\"}";
        let result = super::translate_anthropic_sse_event(data);
        assert!(
            result.is_none(),
            "missing event type line should return None"
        );
    }

    #[test]
    fn translate_anthropic_sse_completes_round_trip_for_full_message() {
        // Simulate a complete message sequence:
        // 1. message_start
        // 2. content_block_start (text)
        // 3. multiple content_block_delta
        // 4. content_block_stop (filtered)
        // 5. message_delta
        // 6. message_stop (filtered)
        let events = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3\"}}",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Hello\"}}",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":12}}",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}",
        ];

        let mut output_count = 0;
        for event in events {
            if super::translate_anthropic_sse_event(event).is_some() {
                output_count += 1;
            }
        }
        // message_start, content_block_start, content_block_delta, message_delta = 4 output events
        // message_stop = filtered
        assert_eq!(
            output_count, 4,
            "expected 4 output events from the sequence"
        );
    }

    // ═══════════════════════════════════════════════════════════════
    //  Token estimation — binary media must not exhaust text TPM
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn estimate_tokens_ignores_stt_base64_audio_file_field() {
        // A large base64 audio blob under the flat `file` field (STT dispatch).
        let audio = "A".repeat(2_560_000); // ~2.5 MB, ~640k "tokens" if counted
        let body = serde_json::json!({
            "model": "whisper-1",
            "file": audio,
            "filename": "audio.wav",
        });
        // Only "whisper-1"/"audio.wav" text is counted → a handful of tokens.
        assert!(
            estimate_tokens(&body) < 100,
            "base64 audio must not be charged as text tokens"
        );
    }

    #[test]
    fn estimate_tokens_ignores_inline_image_data_uri() {
        let data_uri = format!("data:image/png;base64,{}", "Q".repeat(1_000_000));
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe this"},
                    {"type": "image_url", "image_url": {"url": data_uri}}
                ]
            }]
        });
        assert!(
            estimate_tokens(&body) < 100,
            "inline image data URI must not be charged as text tokens"
        );
    }

    #[test]
    fn estimate_tokens_still_counts_real_text() {
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "a".repeat(400)}]
        });
        // ~400 chars of text ≈ 100 tokens; must be materially non-trivial.
        assert!(
            estimate_tokens(&body) >= 100,
            "ordinary text must still be counted"
        );
    }

    #[test]
    fn synthesized_usage_skips_when_provider_reports_usage() {
        let response = serde_json::json!({
            "text": "hi",
            "usage": { "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 }
        });
        assert!(
            synthesized_usage_for_metering(&ModelCapability::Stt, 0, &response).is_none(),
            "existing provider usage must be preserved untouched"
        );
    }

    #[test]
    fn synthesized_usage_skips_text_capabilities() {
        let response = serde_json::json!({ "choices": [] });
        assert!(synthesized_usage_for_metering(&ModelCapability::Chat, 42, &response).is_none());
        assert!(
            synthesized_usage_for_metering(&ModelCapability::Embedding, 42, &response).is_none()
        );
    }

    #[test]
    fn synthesized_usage_bills_tts_by_input_estimate() {
        // TTS output is audio (no tokens); it is billed against the input text.
        let response = serde_json::json!({ "audio": "<binary>" });
        let augmented =
            synthesized_usage_for_metering(&ModelCapability::Tts, 25, &response).unwrap();
        assert_eq!(augmented["usage"]["prompt_tokens"], 25);
        assert_eq!(augmented["usage"]["completion_tokens"], 0);
        assert_eq!(augmented["usage"]["total_tokens"], 25);
    }

    #[test]
    fn synthesized_usage_bills_stt_by_transcription_size() {
        // STT input is audio (no tokens); it is billed against the transcription.
        let response = serde_json::json!({ "text": "a".repeat(40) });
        let augmented =
            synthesized_usage_for_metering(&ModelCapability::Stt, 0, &response).unwrap();
        assert_eq!(augmented["usage"]["prompt_tokens"], 0);
        assert_eq!(augmented["usage"]["completion_tokens"], 10);
        assert_eq!(augmented["usage"]["total_tokens"], 10);
    }

    // ═══════════════════════════════════════════════════════════════
    //  Upstream status classification — client 4xx must not trip breaker
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn client_4xx_is_not_a_channel_health_failure() {
        for code in [400u16, 413, 422] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let err = upstream_status_error(status);
            assert!(
                matches!(err, GatewayError::UpstreamClient { status } if status == code),
                "HTTP {code} should classify as UpstreamClient"
            );
            assert!(
                !err.is_channel_health_failure(),
                "HTTP {code} must not count toward the circuit breaker"
            );
        }
    }

    #[test]
    fn server_5xx_and_429_are_channel_health_failures() {
        for code in [429u16, 500, 502, 503] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let err = upstream_status_error(status);
            assert!(
                matches!(err, GatewayError::Upstream(_)),
                "HTTP {code} should classify as Upstream"
            );
            assert!(
                err.is_channel_health_failure(),
                "HTTP {code} must count toward the circuit breaker"
            );
        }
    }

    #[test]
    fn stream_outcome_client_error_is_neutral_for_breaker() {
        let client_err: Result<StreamOutcome, GatewayError> =
            Err(GatewayError::UpstreamClient { status: 400 });
        assert_eq!(stream_circuit_outcome(&client_err), None);

        let health_err: Result<StreamOutcome, GatewayError> =
            Err(GatewayError::Upstream("HTTP 500".into()));
        assert_eq!(
            stream_circuit_outcome(&health_err),
            Some(CircuitOutcome::Failure)
        );
    }
}
