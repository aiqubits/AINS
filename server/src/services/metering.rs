//! Token metering service for AI Gateway usage accounting.
//!
//! Records per-request token consumption with user_id + tenant_id + channel_id
//! three-dimensional accounting (see AINS_SERVER_PLAN.md 3.5.2).

use std::sync::Arc;

use anyhow::Context;
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    AutoRouter,
    repositories::token_usage::{ActiveModel, Column, Entity, TokenUsageResponse},
};

#[derive(Debug, thiserror::Error)]
pub enum MeteringError {
    #[error("Failed to record usage: {0}")]
    Record(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Token metering service.
///
/// Records AI proxy token consumption and provides query methods for
/// per-user and per-tenant usage aggregation.
#[derive(Clone)]
pub struct MeteringService {
    db: Arc<AutoRouter>,
}

impl MeteringService {
    pub fn new(db: Arc<AutoRouter>) -> Self {
        Self { db }
    }

    /// Record token usage from an upstream response.
    ///
    /// Extracts usage information from the JSON response body
    /// (OpenAI format: `usage.prompt_tokens`, `usage.completion_tokens`,
    /// `usage.total_tokens`) and writes a row to the `token_usage` table.
    pub async fn record_usage(
        &self,
        user_id: i64,
        tenant_id: &str,
        channel_id: Uuid,
        model: &str,
        request_type: &str,
        response: &Value,
    ) -> Result<TokenUsageResponse, MeteringError> {
        let (prompt, completion, total) = extract_usage(response);

        // Skip DB insertion when the response has no "usage" field at all
        // (as opposed to a usage field with zero counters).  This prevents
        // polluting the token_usage table with meaningless zero-usage rows
        // that have no informational value.
        if response.get("usage").is_none() {
            tracing::trace!("Skipping metering insertion: no usage field in response");
            return Ok(TokenUsageResponse {
                id: 0,
                user_id,
                tenant_id: tenant_id.to_string(),
                channel_id,
                model: "unknown".to_string(),
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                request_type: request_type.to_string(),
                created_at: Utc::now(),
            });
        }

        let now = Utc::now();
        let model = if model.is_empty() {
            response
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string()
        } else {
            model.to_string()
        };

        let record = ActiveModel {
            id: Set(crate::snowflake::generate_id()),
            user_id: Set(user_id),
            tenant_id: Set(tenant_id.to_string()),
            channel_id: Set(channel_id),
            model: Set(model),
            prompt_tokens: Set(prompt as i64),
            completion_tokens: Set(completion as i64),
            total_tokens: Set(total as i64),
            request_type: Set(request_type.to_string()),
            created_at: Set(now),
        };

        let result = record
            .insert(self.db.write_conn())
            .await
            .context("insert token usage record")?;

        Ok(result.into())
    }

    /// Get usage for a user, ordered by most recent first.
    pub async fn get_user_usage(
        &self,
        user_id: i64,
        limit: u64,
    ) -> Result<Vec<TokenUsageResponse>, MeteringError> {
        let records = Entity::find()
            .filter(Column::UserId.eq(user_id))
            .order_by_desc(Column::CreatedAt)
            .limit(limit)
            .all(&*self.db)
            .await
            .context("query user token usage")?;
        Ok(records.into_iter().map(Into::into).collect())
    }

    /// Get usage for a tenant, ordered by most recent first.
    pub async fn get_tenant_usage(
        &self,
        tenant_id: &str,
        limit: u64,
    ) -> Result<Vec<TokenUsageResponse>, MeteringError> {
        let records = Entity::find()
            .filter(Column::TenantId.eq(tenant_id))
            .order_by_desc(Column::CreatedAt)
            .limit(limit)
            .all(&*self.db)
            .await
            .context("query tenant token usage")?;
        Ok(records.into_iter().map(Into::into).collect())
    }

    /// Get usage for a specific channel, ordered by most recent first.
    pub async fn get_channel_usage(
        &self,
        channel_id: Uuid,
        limit: u64,
    ) -> Result<Vec<TokenUsageResponse>, MeteringError> {
        let records = Entity::find()
            .filter(Column::ChannelId.eq(channel_id))
            .order_by_desc(Column::CreatedAt)
            .limit(limit)
            .all(&*self.db)
            .await
            .context("query channel token usage")?;
        Ok(records.into_iter().map(Into::into).collect())
    }
}

/// Extract token usage counts from an OpenAI-compatible response body.
///
/// The response typically looks like:
/// ```json
/// {
///   "usage": {
///     "prompt_tokens": 100,
///     "completion_tokens": 50,
///     "total_tokens": 150
///   }
/// }
/// ```
/// For Anthropic responses, the structure uses `input_tokens` and
/// `output_tokens`. Both formats are handled here.
fn extract_usage(response: &Value) -> (u64, u64, u64) {
    let usage = match response.get("usage") {
        Some(u) => u,
        None => return (0, 0, 0),
    };

    // OpenAI format: prompt_tokens, completion_tokens, total_tokens
    let prompt = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| {
            // Anthropic format: input_tokens
            usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        });

    let completion = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| {
            // Anthropic format: output_tokens
            usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        });

    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(prompt + completion);

    (prompt, completion, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_usage_openai_format() {
        let response = json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        });
        let (p, c, t) = extract_usage(&response);
        assert_eq!(p, 100);
        assert_eq!(c, 50);
        assert_eq!(t, 150);
    }

    #[test]
    fn extract_usage_anthropic_format() {
        let response = json!({
            "usage": {
                "input_tokens": 200,
                "output_tokens": 80
            }
        });
        let (p, c, t) = extract_usage(&response);
        assert_eq!(p, 200);
        assert_eq!(c, 80);
        assert_eq!(t, 280); // total = prompt + completion
    }

    #[test]
    fn extract_usage_no_usage_field() {
        let response = json!({"id": "123"});
        let (p, c, t) = extract_usage(&response);
        assert_eq!(p, 0);
        assert_eq!(c, 0);
        assert_eq!(t, 0);
    }

    #[test]
    fn extract_usage_partial() {
        let response = json!({
            "usage": {
                "prompt_tokens": 50
                // no completion_tokens or total_tokens
            }
        });
        let (p, c, t) = extract_usage(&response);
        assert_eq!(p, 50);
        assert_eq!(c, 0);
        assert_eq!(t, 50);
    }
}
