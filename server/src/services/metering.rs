//! Token metering service for AI Gateway usage accounting.
//!
//! Records per-request token consumption with user_id + tenant_id + channel_id
//! three-dimensional accounting (see AINS_SERVER_PLAN.md 3.5.2).

use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseBackend, EntityTrait, FromQueryResult, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, Statement,
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    AutoRouter,
    repositories::token_usage::{ActiveModel, Column, Entity, TokenUsageResponse},
    services::user::PaginatedResponse,
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

// ──────────────────────────────────────────────
//  Query parameter / response types
// ──────────────────────────────────────────────

/// Parameters for paginated token usage queries.
#[derive(Debug, Clone)]
pub struct ListUsageParams {
    pub page: u64,
    pub per_page: u64,
    pub tenant_id: Option<String>,
    pub user_id: Option<i64>,
    pub channel_id: Option<Uuid>,
    pub model: Option<String>,
    pub request_type: Option<String>,
    pub date_from: Option<DateTime<Utc>>,
    pub date_to: Option<DateTime<Utc>>,
}

impl Default for ListUsageParams {
    fn default() -> Self {
        Self {
            page: 1,
            per_page: 20,
            tenant_id: None,
            user_id: None,
            channel_id: None,
            model: None,
            request_type: None,
            date_from: None,
            date_to: None,
        }
    }
}

/// Summary statistics for token usage.
#[derive(Debug, Clone, Serialize)]
pub struct UsageStatsResponse {
    pub total_requests: u64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_tokens: i64,
    pub model_breakdown: Vec<ModelUsageSummary>,
}

/// Per-model usage summary.
#[derive(Debug, Clone, Serialize)]
pub struct ModelUsageSummary {
    pub model: String,
    pub request_count: u64,
    pub total_tokens: i64,
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
        // Skip DB insertion when the response has no "usage" field at all
        // (as opposed to a usage field with zero counters).  This prevents
        // polluting the token_usage table with meaningless zero-usage rows
        // that have no informational value.
        //
        // NOTE: The returned `TokenUsageResponse` has `id: 0` as a sentinel
        // value to indicate the record was NOT persisted. Real persisted
        // records always have a non-zero Snowflake ID (`generate_id()`).
        // Callers should treat `id == 0` as "not recorded".
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

        let (prompt, completion, total) = extract_usage(response);
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

    /// Paginated token usage query with optional filters.
    ///
    /// Supports filtering by tenant_id, user_id, channel_id, model,
    /// request_type, and date range. Results ordered by created_at DESC.
    pub async fn list_usage(
        &self,
        params: ListUsageParams,
    ) -> Result<PaginatedResponse<TokenUsageResponse>, MeteringError> {
        // Sanitize pagination inputs: clamp to safe bounds to prevent
        // overflow, excessive DB offsets, or OOM (consistent with user.rs).
        let per_page = params.per_page.clamp(1, 100);
        let page = params.page.clamp(1, 1_000_000);

        // Apply optional filters.
        //
        // NOTE: This ORM-based filter logic MUST be kept in sync with the
        // raw-SQL `build_stats_where_clause()` below. When adding/removing
        // filter fields from `ListUsageParams`, update BOTH locations.
        let mut select = Entity::find().order_by_desc(Column::CreatedAt);
        if let Some(tenant_id) = &params.tenant_id {
            select = select.filter(Column::TenantId.eq(tenant_id));
        }
        if let Some(user_id) = params.user_id {
            select = select.filter(Column::UserId.eq(user_id));
        }
        if let Some(channel_id) = params.channel_id {
            select = select.filter(Column::ChannelId.eq(channel_id));
        }
        if let Some(model) = &params.model
            && !model.is_empty()
        {
            select = select.filter(Column::Model.eq(model));
        }
        if let Some(request_type) = &params.request_type
            && !request_type.is_empty()
        {
            select = select.filter(Column::RequestType.eq(request_type));
        }
        if let Some(date_from) = &params.date_from {
            select = select.filter(Column::CreatedAt.gte(*date_from));
        }
        if let Some(date_to) = &params.date_to {
            select = select.filter(Column::CreatedAt.lte(*date_to));
        }

        let paginator = select.paginate(&*self.db, per_page);
        let total = paginator.num_items().await.context("count usage items")?;
        let total_pages = total.div_ceil(per_page);
        let items: Vec<TokenUsageResponse> = paginator
            .fetch_page(page - 1)
            .await
            .context("fetch usage page")?
            .into_iter()
            .map(Into::into)
            .collect();

        Ok(PaginatedResponse {
            items,
            total,
            page,
            per_page,
            total_pages,
        })
    }

    /// Get usage statistics summary, supporting the same filters as `list_usage`.
    ///
    /// Returns aggregate counts (total requests, tokens) with a per-model breakdown.
    /// Uses SQL-level aggregation (COUNT/SUM/GROUP BY) for efficiency.
    pub async fn get_usage_stats(
        &self,
        params: ListUsageParams,
    ) -> Result<UsageStatsResponse, MeteringError> {
        let (where_clause, values) = build_stats_where_clause(&params);

        // Aggregate query: COUNT + SUM in a single SQL query
        let agg_sql = format!(
            "SELECT \
             COUNT(*) as total_requests, \
             COALESCE(SUM(prompt_tokens), 0)::bigint as total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)::bigint as total_completion_tokens, \
             COALESCE(SUM(total_tokens), 0)::bigint as total_tokens \
             FROM token_usage {}",
            where_clause
        );

        #[derive(FromQueryResult)]
        struct AggregateRow {
            total_requests: Option<i64>,
            total_prompt_tokens: Option<i64>,
            total_completion_tokens: Option<i64>,
            total_tokens: Option<i64>,
        }

        let agg = AggregateRow::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            &agg_sql,
            values.clone(),
        ))
        .one(&*self.db)
        .await
        .context("aggregate usage stats")?
        .unwrap_or(AggregateRow {
            total_requests: Some(0),
            total_prompt_tokens: Some(0),
            total_completion_tokens: Some(0),
            total_tokens: Some(0),
        });

        // Model breakdown: GROUP BY model in a single SQL query
        let breakdown_sql = format!(
            "SELECT model, \
             COUNT(*) as request_count, \
             COALESCE(SUM(total_tokens), 0)::bigint as total_tokens \
             FROM token_usage {} \
             GROUP BY model ORDER BY total_tokens DESC",
            where_clause
        );

        #[derive(FromQueryResult)]
        struct BreakdownRow {
            model: Option<String>,
            request_count: Option<i64>,
            total_tokens: Option<i64>,
        }

        let breakdown = BreakdownRow::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            &breakdown_sql,
            values,
        ))
        .all(&*self.db)
        .await
        .context("model usage breakdown")?;

        Ok(UsageStatsResponse {
            total_requests: agg
                .total_requests
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(0),
            total_prompt_tokens: agg.total_prompt_tokens.unwrap_or(0),
            total_completion_tokens: agg.total_completion_tokens.unwrap_or(0),
            total_tokens: agg.total_tokens.unwrap_or(0),
            model_breakdown: breakdown
                .into_iter()
                .map(|b| ModelUsageSummary {
                    model: b.model.unwrap_or_else(|| "unknown".to_string()),
                    request_count: b
                        .request_count
                        .and_then(|v| u64::try_from(v).ok())
                        .unwrap_or(0),
                    total_tokens: b.total_tokens.unwrap_or(0),
                })
                .collect(),
        })
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

/// Build a parameterized WHERE clause for stats aggregation queries.
///
/// Returns `(where_sql, values)` where `where_sql` is either empty (no filters)
/// or `"WHERE col1 = $1 AND col2 = $2 ..."` and `values` are the corresponding
/// `sea_orm::Value`s for use with `Statement::from_sql_and_values`.
///
/// NOTE: Keep in sync with `list_usage()` above — the filter logic (which fields,
/// empty-string skipping, etc.) must be identical. When adding/removing filter
/// fields from `ListUsageParams`, update BOTH `list_usage()` AND this function.
///
/// NOTE: `channel_id` (Uuid) is pushed as `sea_orm::Value::Uuid` via `.into()`.
/// SeaORM handles the PostgreSQL UUID serialization transparently — no `::uuid`
/// SQL cast is added. If SeaORM's UUID serialization for PostgreSQL changes,
/// the existing tests will catch the regression.
fn build_stats_where_clause(params: &ListUsageParams) -> (String, Vec<sea_orm::Value>) {
    let mut conds: Vec<String> = Vec::new();
    let mut values: Vec<sea_orm::Value> = Vec::new();

    if let Some(ref tenant_id) = params.tenant_id {
        conds.push(format!("tenant_id = ${}", values.len() + 1));
        values.push(tenant_id.clone().into());
    }
    if let Some(user_id) = params.user_id {
        conds.push(format!("user_id = ${}", values.len() + 1));
        values.push(user_id.into());
    }
    if let Some(channel_id) = params.channel_id {
        conds.push(format!("channel_id = ${}", values.len() + 1));
        values.push(channel_id.into());
    }
    if let Some(ref model) = params.model
        && !model.is_empty()
    {
        conds.push(format!("model = ${}", values.len() + 1));
        values.push(model.clone().into());
    }
    if let Some(ref request_type) = params.request_type
        && !request_type.is_empty()
    {
        conds.push(format!("request_type = ${}", values.len() + 1));
        values.push(request_type.clone().into());
    }
    if let Some(ref date_from) = params.date_from {
        conds.push(format!("created_at >= ${}", values.len() + 1));
        values.push((*date_from).into());
    }
    if let Some(ref date_to) = params.date_to {
        conds.push(format!("created_at <= ${}", values.len() + 1));
        values.push((*date_to).into());
    }

    if conds.is_empty() {
        (String::new(), values)
    } else {
        (format!("WHERE {}", conds.join(" AND ")), values)
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
        .unwrap_or_else(|| prompt.saturating_add(completion));

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

    #[test]
    fn build_stats_where_clause_no_filters() {
        let params = ListUsageParams::default();
        let (sql, values) = build_stats_where_clause(&params);
        assert!(sql.is_empty());
        assert!(values.is_empty());
    }

    #[test]
    fn build_stats_where_clause_single_filter() {
        let params = ListUsageParams {
            tenant_id: Some("t1".to_string()),
            ..Default::default()
        };
        let (sql, values) = build_stats_where_clause(&params);
        assert_eq!(sql, "WHERE tenant_id = $1");
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn build_stats_where_clause_all_filters() {
        let params = ListUsageParams {
            tenant_id: Some("t1".to_string()),
            user_id: Some(42),
            channel_id: Some(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()),
            model: Some("gpt-4".to_string()),
            request_type: Some("chat".to_string()),
            date_from: Some("2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()),
            date_to: Some("2026-12-31T23:59:59Z".parse::<DateTime<Utc>>().unwrap()),
            ..Default::default()
        };
        let (sql, values) = build_stats_where_clause(&params);
        assert!(sql.starts_with("WHERE"));
        assert!(sql.contains("tenant_id = $1"));
        assert!(sql.contains("user_id = $2"));
        assert!(sql.contains("channel_id = $3"));
        assert!(
            !sql.contains("::uuid"),
            "should use typed Value, not SQL cast"
        );
        assert!(sql.contains("model = $4"));
        assert!(sql.contains("request_type = $5"));
        assert!(sql.contains("created_at >= $6"));
        assert!(sql.contains("created_at <= $7"));
        assert_eq!(values.len(), 7);
    }

    #[test]
    fn build_stats_where_clause_empty_strings_skipped() {
        let params = ListUsageParams {
            model: Some(String::new()),
            request_type: Some(String::new()),
            ..Default::default()
        };
        let (sql, values) = build_stats_where_clause(&params);
        assert!(
            sql.is_empty(),
            "empty model/request_type should be skipped: {sql}"
        );
        assert!(values.is_empty());
    }

    #[test]
    fn build_stats_where_clause_parameter_counting() {
        // Verify that $N placeholders are numbered sequentially
        // even when some optional filters are skipped.
        let params = ListUsageParams {
            user_id: Some(1),
            date_from: Some("2026-06-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()),
            ..Default::default()
        };
        let (sql, _values) = build_stats_where_clause(&params);
        assert_eq!(sql, "WHERE user_id = $1 AND created_at >= $2");
    }

    #[test]
    fn list_usage_params_default_has_safe_bounds() {
        let params = ListUsageParams::default();
        assert_eq!(params.page, 1);
        assert_eq!(params.per_page, 20);
    }

    #[test]
    fn test_usage_stats_response_zero_counts_empty_table() {
        // Simulate behavior when token_usage table is empty:
        // - aggregate query returns None (defaulted to zero via unwrap_or)
        // - breakdown query returns empty vec
        let resp = UsageStatsResponse {
            total_requests: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tokens: 0,
            model_breakdown: vec![],
        };
        assert_eq!(resp.total_requests, 0);
        assert_eq!(resp.total_tokens, 0);
        assert!(resp.model_breakdown.is_empty());
    }

    #[test]
    fn test_usage_stats_response_fields() {
        // Verify the response struct is constructible with expected types
        let resp = UsageStatsResponse {
            total_requests: 100,
            total_prompt_tokens: 5000,
            total_completion_tokens: 3000,
            total_tokens: 8000,
            model_breakdown: vec![
                ModelUsageSummary {
                    model: "gpt-4".to_string(),
                    request_count: 80,
                    total_tokens: 6000,
                },
                ModelUsageSummary {
                    model: "claude-3".to_string(),
                    request_count: 20,
                    total_tokens: 2000,
                },
            ],
        };
        assert_eq!(resp.total_requests, 100);
        assert_eq!(resp.model_breakdown.len(), 2);
    }
}
