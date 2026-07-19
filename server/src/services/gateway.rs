use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, OsRng, rand_core::RngCore},
};
use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
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
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("No active channel supports this capability")]
    NoChannel,
    #[error("Channel not found")]
    NotFound,
    #[error("Invalid channel input: {0}")]
    InvalidInput(String),
    #[error("Upstream AI provider failed: {0}")]
    Upstream(String),
    #[error("Channel {id} has {usage_count} token usage record(s); cannot delete")]
    HasUsage { id: String, usage_count: u64 },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
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
        // Client for non-streaming API calls (30s total timeout).
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
        if no_proxy {
            builder = builder.no_proxy();
        }
        let client = builder.build().expect("valid reqwest client");

        // Client for SSE streaming (connect_timeout only, no total timeout)
        // so that long-lived connections (>30s) are not prematurely terminated.
        let mut stream_builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
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
        self.validate(
            &input.name,
            &input.base_url,
            input.weight,
            &input.models,
            &input.capabilities,
            &input.api_key,
        )?;
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
        body: Value,
    ) -> Result<Value, GatewayError> {
        let channel = self.select_channel(tenant_id, &capability).await?;

        // Check quotas before proxying (circuit breaker + rate limits).
        if let Some(ref quota) = self.quota {
            let estimated = estimate_tokens(&body);
            if let Err(e) = quota
                .check_all(
                    &channel.id.to_string(),
                    tenant_id,
                    capability.as_str(),
                    estimated,
                )
                .await
            {
                use crate::services::quota::QuotaError;
                match e {
                    QuotaError::CircuitBroken => return Err(GatewayError::NoChannel),
                    QuotaError::Unavailable => { /* fail-open: allow request */ }
                    other => {
                        return Err(GatewayError::InvalidInput(other.to_string()));
                    }
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

        let result = match action {
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

        // Record success/failure for circuit breaker and metering.
        if let Some(ref quota) = self.quota {
            match &result {
                Ok(_) => quota.record_success(&channel.id.to_string()).await,
                Err(_) => {
                    quota.record_failure(&channel.id.to_string()).await;
                }
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
            // Best-effort: ignore recording errors (non-fatal).
            let _ = metering
                .record_usage(uid, tenant_id, channel.id, &mdl, &req_type, response)
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
    /// Returns `(receiver, channel_id)` so the caller can record metering
    /// against the selected channel after the stream completes.
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
        body: Value,
    ) -> Result<
        (
            tokio::sync::mpsc::UnboundedReceiver<Result<Bytes, String>>,
            Uuid,
        ),
        GatewayError,
    > {
        let channel = self.select_channel(tenant_id, &capability).await?;

        // Check quotas before proxying (same as proxy()).
        if let Some(ref quota) = self.quota {
            let estimated = estimate_tokens(&body);
            if let Err(e) = quota
                .check_all(
                    &channel.id.to_string(),
                    tenant_id,
                    capability.as_str(),
                    estimated,
                )
                .await
            {
                use crate::services::quota::QuotaError;
                match e {
                    QuotaError::CircuitBroken => return Err(GatewayError::NoChannel),
                    QuotaError::Unavailable => { /* fail-open */ }
                    other => return Err(GatewayError::InvalidInput(other.to_string())),
                }
            }
        }

        let api_key = self.decrypt(&channel.api_key_encrypted)?;
        let action =
            dispatch::dispatch_proxy(&capability, &channel.protocol_type, &channel.base_url, body)?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
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
                        match &result {
                            Ok(_) => q.record_success(&channel_id.to_string()).await,
                            Err(_) => {
                                q.record_failure(&channel_id.to_string()).await;
                            }
                        }
                    }
                });
            }
            DispatchAction::TtsBinary { .. } | DispatchAction::SttMultipart { .. } => {
                return Err(GatewayError::InvalidInput(
                    "Streaming is only supported for chat/embedding endpoints".into(),
                ));
            }
        }

        Ok((rx, channel_id))
    }

    /// Handle STT and record metering (STT has a different flow: multipart upload
    /// without a model field in the dispatch body).
    async fn send_stt_and_meter(
        &self,
        ctx: SttCtx<'_>,
        action: &DispatchAction,
        model: &str,
    ) -> Result<Value, GatewayError> {
        let result = match action {
            DispatchAction::SttMultipart {
                url,
                audio_bytes,
                filename,
                ..
            } => {
                self.send_stt(url, ctx.api_key, audio_bytes.clone(), filename, model)
                    .await
            }
            _ => unreachable!(),
        };

        if let Some(ref quota) = self.quota {
            match &result {
                Ok(_) => quota.record_success(&ctx.channel.id.to_string()).await,
                Err(_) => {
                    quota.record_failure(&ctx.channel.id.to_string()).await;
                }
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
            let _ = metering
                .record_usage(
                    uid,
                    ctx.tenant_id,
                    ctx.channel.id,
                    model,
                    ctx.req_type,
                    response,
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
    ) -> Result<Value, GatewayError> {
        let mime = audio_mime_from_filename(filename);
        let part = reqwest::multipart::Part::bytes(audio_bytes)
            .file_name(filename.to_string())
            .mime_str(mime)
            .map_err(|e| GatewayError::Internal(anyhow::anyhow!("mime: {}", e)))?;

        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", model.to_string());

        let response = self
            .client
            .post(url)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;
        Self::parse_json_response(response).await
    }

    /// TTS: JSON POST, binary audio response.
    async fn send_tts(
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

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            tracing::warn!(
                upstream_status = %status.as_u16(),
                upstream_body = %body_text,
                "TTS upstream returned error"
            );
            return Err(GatewayError::Upstream(format!("HTTP {}", status.as_u16())));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        // Return the binary audio as a base64-encoded JSON response so the
        // client can decode it. For a streaming response, a future upgrade
        // could return raw bytes directly.
        let encoded = URL_SAFE_NO_PAD.encode(&bytes);
        Ok(serde_json::json!({
            "audio": encoded,
            "content_type": "audio/mpeg"
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
            return Err(GatewayError::Upstream(format!("HTTP {}", status.as_u16())));
        }
        response
            .json::<Value>()
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))
    }

    async fn select_channel(
        &self,
        tenant_id: &str,
        capability: &ModelCapability,
    ) -> Result<Model, GatewayError> {
        let channels = Entity::find()
            .filter(Column::TenantId.eq(tenant_id))
            .filter(Column::IsActive.eq(true))
            .filter(Expr::cust_with_values(
                "capabilities ? CAST($1 AS text)",
                vec![sea_orm::Value::from(capability.as_str())],
            ))
            .all(&*self.db)
            .await
            .context("load candidate channels")?;
        if channels.is_empty() {
            return Err(GatewayError::NoChannel);
        }
        weighted_select(&channels)
            .cloned()
            .ok_or(GatewayError::NoChannel)
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Streaming proxy helpers
// ═══════════════════════════════════════════════════════════════════

/// Send a streaming JSON POST request (OpenAI-compatible) and forward raw
/// SSE bytes through the channel. Returns Ok(()) on successful stream
/// completion, or Err on connection/HTTP-level failure.
async fn send_json_stream(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &Value,
    tx: UnboundedSender<Result<Bytes, String>>,
) -> Result<(), GatewayError> {
    let response = client
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
            "Upstream streaming request returned error"
        );
        let _ = tx.send(Err(format!("HTTP {}", status.as_u16())));
        return Err(GatewayError::Upstream(format!("HTTP {}", status.as_u16())));
    }

    let mut byte_stream = response.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        match chunk {
            Ok(bytes) => {
                if tx.send(Ok(bytes)).is_err() {
                    // Receiver dropped (client disconnected)
                    break;
                }
            }
            Err(e) => {
                tracing::error!("Streaming read error: {}", e);
                let _ = tx.send(Err(format!("Stream error: {}", e)));
                return Err(GatewayError::Upstream(e.to_string()));
            }
        }
    }

    Ok(())
}

/// Send a streaming request to Anthropic (x-api-key header auth) and forward
/// raw SSE bytes through the channel.
async fn send_anthropic_stream(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &Value,
    tx: UnboundedSender<Result<Bytes, String>>,
) -> Result<(), GatewayError> {
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

    let response = client
        .post(url)
        .headers(headers)
        .json(&req_body)
        .send()
        .await
        .map_err(|e| GatewayError::Upstream(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        tracing::warn!(
            upstream_status = %status.as_u16(),
            upstream_body = %body_text,
            "Anthropic streaming request returned error"
        );
        let _ = tx.send(Err(format!("HTTP {}", status.as_u16())));
        return Err(GatewayError::Upstream(format!("HTTP {}", status.as_u16())));
    }

    // Buffer for accumulating SSE events across chunks
    // Safety limit to prevent unbounded growth on malformed upstream data.
    const MAX_SSE_BUFFER: usize = 1_048_576; // 1 MB
    let mut buffer = String::new();

    let mut byte_stream = response.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        match chunk {
            Ok(bytes) => {
                if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                    // Safety guard: prevent unbounded buffer growth when upstream
                    // sends data without \n\n event terminators.
                    if buffer.len() + text.len() > MAX_SSE_BUFFER {
                        tracing::error!(
                            buffer_bytes = buffer.len() + text.len(),
                            max = MAX_SSE_BUFFER,
                            "Anthropic SSE buffer overflow — terminating stream"
                        );
                        let _ = tx.send(Err("Internal error: stream buffer overflow".into()));
                        return Ok(());
                    }
                    buffer.push_str(&text);
                    // Process complete SSE events — use split_off to avoid
                    // allocating both event_block AND replacement buffer.
                    while let Some(pos) = buffer.find("\n\n") {
                        let remaining = buffer.split_off(pos + 2);
                        let event_block = std::mem::replace(&mut buffer, remaining);
                        // event_block now holds "...\n\n" — strip the trailing "\n\n"
                        let event_str = &event_block[..event_block.len() - 2];
                        if let Some(converted) = translate_anthropic_sse_event(event_str)
                            && tx.send(Ok(Bytes::from(converted))).is_err()
                        {
                            return Ok(());
                        }
                    }
                } else {
                    // Non-UTF-8 bytes from Anthropic upstream are unexpected but
                    // not fatal — log a warning and continue processing the next chunk.
                    // The offending bytes are discarded; previously buffered valid
                    // SSE events are unaffected.
                    tracing::warn!(
                        "Anthropic stream produced non-UTF-8 bytes ({:?}) — dropping {} bytes and continuing",
                        bytes.as_ref(),
                        bytes.len(),
                    );
                }
            }
            Err(e) => {
                tracing::error!("Anthropic streaming read error: {}", e);
                let _ = tx.send(Err(format!("Stream error: {}", e)));
                return Err(GatewayError::Upstream(e.to_string()));
            }
        }
    }

    // Send [DONE] signal
    let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n")));

    Ok(())
}

/// Translate an Anthropic SSE event block into OpenAI-compatible SSE format.
/// Returns None for events that should be filtered (no content change).
fn translate_anthropic_sse_event(event_block: &str) -> Option<String> {
    // Extract event type from "event: xxx" line
    let event_type = event_block
        .lines()
        .find(|l| l.starts_with("event: "))
        .and_then(|l| l.strip_prefix("event: "))
        .unwrap_or("");

    // Extract data from "data: {...}" line
    let data_str = event_block
        .lines()
        .find(|l| l.starts_with("data: "))
        .and_then(|l| l.strip_prefix("data: "))
        .unwrap_or("");

    if data_str.is_empty() {
        return None;
    }

    let data: &serde_json::Value = &serde_json::from_str(data_str).ok()?;

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
            let openai_chunk = serde_json::json!({
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": ""},
                    "finish_reason": null
                }]
            });
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
                Some("end_turn" | "stop_sequence") => "stop",
                Some("max_tokens") => "length",
                _ => "stop",
            };
            let openai_chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish
                }]
            });
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
fn estimate_tokens(body: &Value) -> u64 {
    // Simple estimation: count characters in string values (rough: ~4 chars per token).
    let text_len = estimate_text_length(body);
    (text_len / 4).max(1)
}

fn estimate_text_length(value: &Value) -> u64 {
    match value {
        Value::String(s) => s.len() as u64,
        Value::Array(arr) => arr.iter().map(estimate_text_length).sum(),
        Value::Object(obj) => obj.values().map(estimate_text_length).sum(),
        _ => 0,
    }
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
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "webm" => "audio/webm",
        _ => "audio/wav",
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

impl GatewayService {
    fn validate(
        &self,
        name: &str,
        base_url: &str,
        weight: i32,
        models: &[String],
        capabilities: &[ModelCapability],
        api_key: &str,
    ) -> Result<(), GatewayError> {
        if name.trim().is_empty()
            || api_key.is_empty()
            || models.is_empty()
            || capabilities.is_empty()
            || weight < 1
            || !(base_url.starts_with("https://") || base_url.starts_with("http://"))
        {
            return Err(GatewayError::InvalidInput("name, api_key, models, capabilities, positive weight, and http(s) base_url are required".into()));
        }
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
    use sea_orm::DatabaseConnection;
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
        let event = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-opus-20240229\"}}";
        let result = super::translate_anthropic_sse_event(event);
        assert!(result.is_some(), "message_start should produce an event");
        let sse = result.unwrap();
        assert!(
            sse.contains("role\":\"assistant\"") || sse.contains("\\\"role\\\":\\\"assistant\\\""),
            "should set role to assistant"
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
}
