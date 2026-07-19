//! Token usage query handlers for the admin API.
//!
//! Provides paginated token usage listing and aggregate stats,
//! with filtering and tenant-scoped access control.

use crate::handlers::helpers::extract_handler_context;
use crate::repositories::token_usage::TokenUsageResponse;
use crate::services::metering::{ListUsageParams, MeteringService};
use crate::services::user::PaginatedResponse;
use ains_runtime::{HttpError, RequestContext, Response};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

/// Shared filter fields for token usage query endpoints.
///
/// Used via `#[serde(flatten)]` in both `ListUsageQuery` and `UsageStatsQuery`
/// to avoid duplicating the 7 filter fields across both types.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct UsageFilters {
    pub tenant_id: Option<String>,
    /// Accepted as a String (not a typed `i64`) on purpose: query strings are
    /// deserialized via `serde_urlencoded`, which cannot deserialize numeric
    /// types inside a `#[serde(flatten)]` struct (it fails with
    /// "invalid type: string, expected i64"). We parse it to `i64` in
    /// `parse_user_id` instead. See `build_params`.
    pub user_id: Option<String>,
    pub channel_id: Option<Uuid>,
    pub model: Option<String>,
    pub request_type: Option<String>,
    /// ISO 8601 date string (e.g. "2026-01-01T00:00:00Z" or "2026-01-01")
    pub date_from: Option<String>,
    /// ISO 8601 date string
    pub date_to: Option<String>,
}

/// Query parameters for `GET /api/usage`
#[derive(Debug, Deserialize)]
pub struct ListUsageQuery {
    #[serde(default = "default_page")]
    pub page: u64,
    #[serde(default = "default_per_page")]
    pub per_page: u64,
    #[serde(flatten)]
    pub filters: UsageFilters,
}

impl Default for ListUsageQuery {
    fn default() -> Self {
        Self {
            page: default_page(),
            per_page: default_per_page(),
            filters: UsageFilters::default(),
        }
    }
}

/// Query parameters for `GET /api/usage/stats`
#[derive(Debug, Deserialize, Default)]
pub struct UsageStatsQuery {
    #[serde(flatten)]
    pub filters: UsageFilters,
}

fn default_page() -> u64 {
    1
}
fn default_per_page() -> u64 {
    20
}

/// Parse an ISO 8601 date/time string into chrono::DateTime<Utc>.
///
/// Accepts full ISO 8601 datetime (e.g. "2026-01-01T00:00:00Z") as well as
/// bare date strings (e.g. "2026-01-01"). See `end_of_day` for how bare dates
/// are expanded.
///
/// `end_of_day` controls how a *bare* date is expanded: when `false` it maps to
/// start-of-day (00:00:00), when `true` it maps to end-of-day
/// (23:59:59.999999). This keeps an inclusive `[date_from, date_to]` range —
/// otherwise a bare `date_to` like "2026-12-31" would collapse to 00:00:00 and
/// silently exclude almost the entire final day. Full ISO 8601 datetimes are
/// used verbatim regardless of `end_of_day`.
fn parse_date(
    s: Option<String>,
    end_of_day: bool,
) -> Result<Option<chrono::DateTime<Utc>>, HttpError> {
    match s {
        None => Ok(None),
        Some(val) => {
            let val = val.trim().to_string();
            // Treat blank as "no filter" (consistent with parse_user_id)
            if val.is_empty() {
                return Ok(None);
            }
            // Try full ISO 8601 datetime first
            match val.parse::<chrono::DateTime<Utc>>() {
                Ok(dt) => Ok(Some(dt)),
                Err(_) => {
                    // Try bare date format: "2026-01-01" → start or end of day
                    match chrono::NaiveDate::parse_from_str(&val, "%Y-%m-%d") {
                        Ok(naive_date) => {
                            let naive_time = if end_of_day {
                                naive_date
                                    .and_hms_micro_opt(23, 59, 59, 999_999)
                                    .expect("23:59:59.999999 is a valid time")
                            } else {
                                naive_date
                                    .and_hms_opt(0, 0, 0)
                                    .expect("00:00:00 is a valid time")
                            };
                            let dt: chrono::DateTime<Utc> = naive_time.and_utc();
                            Ok(Some(dt))
                        }
                        Err(_) => Err(HttpError::bad_request(format!(
                            "Invalid date/time format: expected ISO 8601 (e.g. 2026-01-01T00:00:00Z) or date (e.g. 2026-01-01), got '{val}'"
                        ))),
                    }
                }
            }
        }
    }
}

/// Parse an optional `user_id` query string into `i64`.
///
/// `user_id` arrives as a String (see `UsageFilters`) because `serde_urlencoded`
/// cannot deserialize numeric types inside a flattened struct. An empty/blank
/// value is treated as "no filter" (consistent with the empty-string skipping
/// applied to `model`/`request_type`).
fn parse_user_id(s: Option<String>) -> Result<Option<i64>, HttpError> {
    match s {
        None => Ok(None),
        Some(val) => {
            let val = val.trim();
            if val.is_empty() {
                return Ok(None);
            }
            val.parse::<i64>().map(Some).map_err(|_| {
                HttpError::bad_request(format!("Invalid user_id: expected an integer, got '{val}'"))
            })
        }
    }
}

/// Convert a `ListUsageQuery` into `ListUsageParams`, enforcing tenant scope.
fn build_params(
    query: ListUsageQuery,
    actor_role: &str,
    actor_tenant_id: &str,
) -> Result<ListUsageParams, HttpError> {
    let f = &query.filters;

    // Trim string filter values for consistency (parse_date / parse_user_id both trim).
    let trimmed_tenant_id = f.tenant_id.as_ref().map(|v| v.trim().to_string());
    let trimmed_model = f.model.as_ref().map(|v| v.trim().to_string());
    let trimmed_request_type = f.request_type.as_ref().map(|v| v.trim().to_string());

    // Enforce tenant scope: admin can only see their own tenant.
    // If actor_tenant_id is empty (pre-migration JWT without tenant_id claim),
    // fail with a clear message instead of silently returning empty results.
    let tenant_id = if actor_role == "system" {
        // Treat empty/blank tenant_id as "no filter" (consistent with parse_user_id).
        // A system user sending `?tenant_id=` (empty) will see all tenants,
        // not filter by `tenant_id = ''` which would silently return nothing.
        trimmed_tenant_id.filter(|v| !v.is_empty())
    } else {
        if actor_tenant_id.is_empty() {
            return Err(HttpError::bad_request(
                "Token missing tenant_id claim; please re-login",
            ));
        }
        Some(actor_tenant_id.to_string())
    };

    // Sanitize pagination: page [1, 1_000_000], per_page [1, 100]
    let page = query.page.clamp(1, 1_000_000);
    let per_page = query.per_page.clamp(1, 100);

    let date_from = parse_date(f.date_from.clone(), false)?;
    let date_to = parse_date(f.date_to.clone(), true)?;

    // Reject inverted date ranges with a clear error message.
    // Without this check, `date_from > date_to` silently returns zero
    // records, which looks like an empty data set rather than a bad input.
    if let (Some(from), Some(to)) = (&date_from, &date_to)
        && from > to
    {
        return Err(HttpError::bad_request(format!(
            "date_from ({from}) must not be later than date_to ({to})"
        )));
    }

    Ok(ListUsageParams {
        page,
        per_page,
        tenant_id,
        user_id: parse_user_id(f.user_id.clone())?,
        channel_id: f.channel_id,
        model: trimmed_model.filter(|v| !v.is_empty()),
        request_type: trimmed_request_type.filter(|v| !v.is_empty()),
        date_from,
        date_to,
    })
}

/// Build `ListUsageParams` for stats endpoint (no pagination).
///
/// Delegates to `build_params` to avoid duplicating tenant-scoping and
/// date-parsing logic. Pagination values are ignored by the stats query.
fn build_stats_params(
    query: UsageStatsQuery,
    actor_role: &str,
    actor_tenant_id: &str,
) -> Result<ListUsageParams, HttpError> {
    let paginated = ListUsageQuery {
        page: 1,
        per_page: 20,
        filters: query.filters,
    };
    build_params(paginated, actor_role, actor_tenant_id)
}

/// Paginated token usage listing — `GET /api/usage`
///
/// Requires admin/system role. Admins are scoped to their own tenant.
pub async fn list_token_usage(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, auth_user) = extract_handler_context(&req)?;
    if auth_user.role != "system" && auth_user.role != "admin" {
        return Err(HttpError::forbidden("Admin or system role required"));
    }
    let query: ListUsageQuery = req
        .parse_query()
        .map_err(|e| HttpError::bad_request(format!("Invalid query parameters: {e}")))?;
    let params = build_params(query, &auth_user.role, &auth_user.tenant_id)?;

    let service = MeteringService::new(state.db);
    let result: PaginatedResponse<TokenUsageResponse> =
        service.list_usage(params).await.map_err(|e| {
            tracing::error!(
                error = ?e,
                user_id = auth_user.user_id,
                tenant_id = auth_user.tenant_id,
                endpoint = "/api/usage",
                "Failed to query token usage"
            );
            HttpError::internal("Failed to query token usage")
        })?;

    Response::json(&result)
}

/// Token usage statistics — `GET /api/usage/stats`
///
/// Requires admin/system role. Admins are scoped to their own tenant.
pub async fn get_token_usage_stats(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, auth_user) = extract_handler_context(&req)?;
    if auth_user.role != "system" && auth_user.role != "admin" {
        return Err(HttpError::forbidden("Admin or system role required"));
    }
    let query: UsageStatsQuery = req
        .parse_query()
        .map_err(|e| HttpError::bad_request(format!("Invalid query parameters: {e}")))?;
    let params = build_stats_params(query, &auth_user.role, &auth_user.tenant_id)?;

    let service = MeteringService::new(state.db);
    let stats = service.get_usage_stats(params).await.map_err(|e| {
        tracing::error!(
            error = ?e,
            user_id = auth_user.user_id,
            tenant_id = auth_user.tenant_id,
            endpoint = "/api/usage/stats",
            "Failed to query usage stats"
        );
        HttpError::internal("Failed to query usage stats")
    })?;

    Response::json(&stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn test_parse_date_none() {
        assert!(parse_date(None, false).unwrap().is_none());
    }

    #[test]
    fn test_parse_date_full_iso() {
        let dt = parse_date(Some("2026-01-15T10:30:00Z".to_string()), false)
            .unwrap()
            .unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 15);
        assert_eq!(dt.hour(), 10);
        assert_eq!(dt.minute(), 30);
    }

    #[test]
    fn test_parse_date_full_iso_ignores_end_of_day() {
        // A full ISO datetime is used verbatim regardless of the end_of_day flag.
        let dt = parse_date(Some("2026-01-15T10:30:00Z".to_string()), true)
            .unwrap()
            .unwrap();
        assert_eq!(dt.hour(), 10);
        assert_eq!(dt.minute(), 30);
        assert_eq!(dt.second(), 0);
    }

    #[test]
    fn test_parse_date_bare_date() {
        // Bare date "2026-07-01" with end_of_day=false should be start of day.
        let dt = parse_date(Some("2026-07-01".to_string()), false)
            .unwrap()
            .unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 7);
        assert_eq!(dt.day(), 1);
        assert_eq!(dt.hour(), 0);
        assert_eq!(dt.minute(), 0);
        assert_eq!(dt.second(), 0);
    }

    #[test]
    fn test_parse_date_bare_date_end_of_day() {
        // Bare date with end_of_day=true should expand to 23:59:59 so that an
        // inclusive date_to does not silently drop the final day.
        let dt = parse_date(Some("2026-07-01".to_string()), true)
            .unwrap()
            .unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 7);
        assert_eq!(dt.day(), 1);
        assert_eq!(dt.hour(), 23);
        assert_eq!(dt.minute(), 59);
        assert_eq!(dt.second(), 59);
    }

    #[test]
    fn test_parse_date_trim_whitespace() {
        let dt = parse_date(Some("  2026-07-01  ".to_string()), false)
            .unwrap()
            .unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 7);
        assert_eq!(dt.day(), 1);
    }

    #[test]
    fn test_parse_date_invalid() {
        let err = parse_date(Some("not-a-date".to_string()), false).unwrap_err();
        assert_eq!(err.status.as_u16(), 400);
    }

    #[test]
    fn test_parse_date_empty_string_is_no_filter() {
        // Empty string should be treated as "no filter" (consistent with parse_user_id).
        assert!(parse_date(Some(String::new()), false).unwrap().is_none());
        assert!(
            parse_date(Some("   ".to_string()), false)
                .unwrap()
                .is_none()
        );
    }

    // ──────────────────────────────────────────────
    //  parse_user_id
    // ──────────────────────────────────────────────

    #[test]
    fn test_parse_user_id_none() {
        assert!(parse_user_id(None).unwrap().is_none());
    }

    #[test]
    fn test_parse_user_id_valid() {
        assert_eq!(parse_user_id(Some("42".to_string())).unwrap(), Some(42));
        // Snowflake-sized ids must fit in i64.
        assert_eq!(
            parse_user_id(Some("1234567890123456789".to_string())).unwrap(),
            Some(1_234_567_890_123_456_789)
        );
    }

    #[test]
    fn test_parse_user_id_trims_and_skips_empty() {
        assert_eq!(parse_user_id(Some("  7  ".to_string())).unwrap(), Some(7));
        // Blank value is treated as "no filter", not an error.
        assert!(parse_user_id(Some("   ".to_string())).unwrap().is_none());
        assert!(parse_user_id(Some(String::new())).unwrap().is_none());
    }

    #[test]
    fn test_parse_user_id_invalid() {
        let err = parse_user_id(Some("abc".to_string())).unwrap_err();
        assert_eq!(err.status.as_u16(), 400);
    }

    // ──────────────────────────────────────────────
    //  serde_urlencoded query deserialization (regression)
    // ──────────────────────────────────────────────

    // Regression guard: `serde_urlencoded` cannot deserialize numeric types
    // inside a `#[serde(flatten)]` struct (it errors with
    // "invalid type: string, expected i64"). `user_id` MUST therefore be a
    // String in `UsageFilters`. This test fails if it is ever changed back to
    // a typed integer, or if any other numeric field is added to the flattened
    // filters.
    #[test]
    fn test_list_usage_query_deserializes_all_filters() {
        let q: ListUsageQuery = serde_urlencoded::from_str(
            "page=2&per_page=50&user_id=42&\
             channel_id=550e8400-e29b-41d4-a716-446655440000&\
             model=gpt-4&request_type=chat&date_from=2026-01-01&date_to=2026-12-31",
        )
        .expect("query with all filters must deserialize via serde_urlencoded");
        assert_eq!(q.page, 2);
        assert_eq!(q.per_page, 50);
        assert_eq!(q.filters.user_id.as_deref(), Some("42"));
        assert_eq!(q.filters.model.as_deref(), Some("gpt-4"));
        assert_eq!(q.filters.request_type.as_deref(), Some("chat"));
        assert!(q.filters.channel_id.is_some());
        assert_eq!(q.filters.date_from.as_deref(), Some("2026-01-01"));
        assert_eq!(q.filters.date_to.as_deref(), Some("2026-12-31"));
    }

    #[test]
    fn test_stats_query_deserializes_user_id() {
        let q: UsageStatsQuery = serde_urlencoded::from_str("user_id=7&model=claude-3")
            .expect("stats query with user_id must deserialize");
        assert_eq!(q.filters.user_id.as_deref(), Some("7"));
        assert_eq!(q.filters.model.as_deref(), Some("claude-3"));
    }

    #[test]
    fn test_list_usage_query_empty_deserializes_to_defaults() {
        let q: ListUsageQuery = serde_urlencoded::from_str("").expect("empty query is valid");
        assert_eq!(q.page, 1);
        assert_eq!(q.per_page, 20);
        assert!(q.filters.user_id.is_none());
    }

    #[test]
    fn test_build_params_parses_user_id() {
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                user_id: Some("99".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "t1").unwrap();
        assert_eq!(params.user_id, Some(99));
    }

    #[test]
    fn test_build_params_rejects_invalid_user_id() {
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                user_id: Some("not-a-number".to_string()),
                ..UsageFilters::default()
            },
        };
        let err = build_params(query, "system", "t1").unwrap_err();
        assert_eq!(err.status.as_u16(), 400);
    }

    #[test]
    fn test_build_params_date_to_bare_is_end_of_day() {
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                date_from: Some("2026-07-01".to_string()),
                date_to: Some("2026-07-31".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "t1").unwrap();
        let from = params.date_from.unwrap();
        let to = params.date_to.unwrap();
        // date_from is start-of-day, date_to is end-of-day → inclusive range.
        assert_eq!(from.hour(), 0);
        assert_eq!(to.hour(), 23);
        assert_eq!(to.minute(), 59);
        assert_eq!(to.second(), 59);
    }

    // ──────────────────────────────────────────────
    //  build_params & build_stats_params
    // ──────────────────────────────────────────────

    #[test]
    fn test_build_params_system_can_specify_tenant() {
        let query = ListUsageQuery {
            page: 2,
            per_page: 50,
            filters: UsageFilters {
                tenant_id: Some("tenant-abc".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "own-tenant").unwrap();
        assert_eq!(params.tenant_id, Some("tenant-abc".to_string()));
    }

    #[test]
    fn test_build_params_admin_forced_to_own_tenant() {
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                tenant_id: Some("other-tenant".to_string()),
                ..UsageFilters::default()
            },
        };
        // Admin requests other-tenant but is forced to own-tenant
        let params = build_params(query, "admin", "own-tenant").unwrap();
        assert_eq!(params.tenant_id, Some("own-tenant".to_string()));
    }

    #[test]
    fn test_build_params_clamps_page_lower_bound() {
        let query = ListUsageQuery {
            page: 0,
            per_page: 0,
            ..ListUsageQuery::default()
        };
        let params = build_params(query, "admin", "t1").unwrap();
        assert_eq!(params.page, 1, "page=0 should be clamped to 1");
        assert_eq!(params.per_page, 1, "per_page=0 should be clamped to 1");
    }

    #[test]
    fn test_build_params_clamps_page_overflow_large() {
        let query = ListUsageQuery {
            page: u64::MAX,
            per_page: u64::MAX,
            ..ListUsageQuery::default()
        };
        let params = build_params(query, "admin", "t1").unwrap();
        assert_eq!(
            params.page, 1_000_000,
            "page=u64::MAX should be clamped to 1_000_000"
        );
        assert_eq!(
            params.per_page, 100,
            "per_page=u64::MAX should be clamped to 100"
        );
    }

    #[test]
    fn test_build_params_clamps_page_upper_bound() {
        let query = ListUsageQuery {
            page: 9_999_999,
            per_page: 9999,
            ..ListUsageQuery::default()
        };
        let params = build_params(query, "admin", "t1").unwrap();
        assert_eq!(params.page, 1_000_000);
        assert_eq!(params.per_page, 100);
    }

    #[test]
    fn test_build_params_system_none_tenant_id() {
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                tenant_id: None,
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "t1").unwrap();
        assert!(
            params.tenant_id.is_none(),
            "system with None tenant_id should pass through"
        );
    }

    #[test]
    fn test_build_stats_params_admin_scoped() {
        let query = UsageStatsQuery {
            filters: UsageFilters {
                tenant_id: Some("other".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_stats_params(query, "admin", "my-tenant").unwrap();
        assert_eq!(params.tenant_id, Some("my-tenant".to_string()));
    }

    #[test]
    fn test_build_stats_params_system_passthrough() {
        let query = UsageStatsQuery {
            filters: UsageFilters {
                tenant_id: Some("cross-tenant".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_stats_params(query, "system", "my-tenant").unwrap();
        assert_eq!(params.tenant_id, Some("cross-tenant".to_string()));
    }

    #[test]
    fn test_build_stats_params_default_pagination() {
        // After refactoring, build_stats_params delegates to build_params.
        // Verify it still produces correct default pagination (stats ignores these).
        let query = UsageStatsQuery::default();
        let params = build_stats_params(query, "admin", "t1").unwrap();
        assert_eq!(params.page, 1);
        assert_eq!(params.per_page, 20);
    }

    #[test]
    fn test_default_page_per_page() {
        assert_eq!(default_page(), 1);
        assert_eq!(default_per_page(), 20);
    }

    #[test]
    fn test_build_params_rejects_inverted_date_range() {
        // date_from (Dec 31) > date_to (Jan 1) — should be rejected with a 400.
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                date_from: Some("2026-12-31".to_string()),
                date_to: Some("2026-01-01".to_string()),
                ..UsageFilters::default()
            },
        };
        let err = build_params(query, "system", "t1").unwrap_err();
        assert_eq!(err.status.as_u16(), 400);
        assert!(
            err.message.contains("date_from") && err.message.contains("date_to"),
            "error should mention date_from and date_to: {}",
            err.message
        );
    }

    #[test]
    fn test_build_params_allows_same_day_range() {
        // date_from == date_to (same day) — valid for single-day reports.
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                date_from: Some("2026-07-01".to_string()),
                date_to: Some("2026-07-01".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "t1").unwrap();
        // date_from expanded to start-of-day (00:00:00), date_to to end-of-day (23:59:59.999999)
        // so from <= to is satisfied.
        assert!(params.date_from.unwrap() <= params.date_to.unwrap());
    }

    // ──────────────────────────────────────────────
    //  Edge cases & error paths
    // ──────────────────────────────────────────────

    #[test]
    fn test_build_params_empty_tenant_id_rejected() {
        // Admin with empty tenant_id (old JWT without tenant_id claim) → 400
        let query = ListUsageQuery::default();
        let err = build_params(query, "admin", "").unwrap_err();
        assert_eq!(err.status.as_u16(), 400);
        assert!(
            err.message.contains("tenant_id"),
            "error should mention tenant_id: {}",
            err.message
        );
    }

    #[test]
    fn test_build_params_system_empty_tenant_id_passthrough() {
        // System with empty tenant_id is fine (not tenant-scoped)
        let query = ListUsageQuery::default();
        let params = build_params(query, "system", "").unwrap();
        assert!(params.tenant_id.is_none());
    }

    #[test]
    fn test_build_params_system_blank_tenant_id_string() {
        // System user sending `?tenant_id=` (empty string) should be treated as None.
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                tenant_id: Some(String::new()),
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "t1").unwrap();
        assert!(
            params.tenant_id.is_none(),
            "empty tenant_id string should be treated as no filter"
        );
    }

    #[test]
    fn test_build_params_system_whitespace_tenant_id_string() {
        // System user sending `?tenant_id=+%20+` (whitespace-only) should be treated as None.
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                tenant_id: Some("   ".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "t1").unwrap();
        assert!(
            params.tenant_id.is_none(),
            "whitespace tenant_id string should be treated as no filter"
        );
    }

    #[test]
    fn test_build_params_trims_model_and_request_type() {
        // Leading/trailing whitespace in model/request_type should be trimmed.
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                model: Some("  gpt-4  ".to_string()),
                request_type: Some("\tchat\n".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "t1").unwrap();
        assert_eq!(
            params.model.as_deref(),
            Some("gpt-4"),
            "model should be trimmed"
        );
        assert_eq!(
            params.request_type.as_deref(),
            Some("chat"),
            "request_type should be trimmed"
        );
    }

    #[test]
    fn test_build_params_empty_model_becomes_none() {
        // Empty/whitespace-only model should be treated as "no filter" (None).
        let query = ListUsageQuery {
            page: 1,
            per_page: 20,
            filters: UsageFilters {
                model: Some(String::new()),
                request_type: Some("   ".to_string()),
                ..UsageFilters::default()
            },
        };
        let params = build_params(query, "system", "t1").unwrap();
        assert!(params.model.is_none(), "empty model should become None");
        assert!(
            params.request_type.is_none(),
            "whitespace request_type should become None"
        );
    }

    #[test]
    fn test_list_usage_query_deserializes_invalid_channel_id() {
        // Invalid channel_id (not a UUID) should fail deserialization
        let result: Result<ListUsageQuery, _> =
            serde_urlencoded::from_str("page=1&per_page=10&channel_id=not-a-uuid");
        assert!(
            result.is_err(),
            "invalid channel_id should fail deserialization"
        );
    }
}
