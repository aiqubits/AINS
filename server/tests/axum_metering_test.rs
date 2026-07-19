#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for Token Metering (AINS_SERVER_PLAN §0.13).
//!
//! Tests MeteringService directly against a real PostgreSQL database,
//! verifying the 3D accounting dimensions (user_id + tenant_id + channel_id).
//!
//! These tests require running PostgreSQL and Redis instances.
//! Run: cargo test --test axum_metering_test

use ains_axum::{Body, BodyExt, Method, StatusCode};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

/// Generate a unique test base ID for per-test isolation against parallel runs.
fn unique_base() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

/// Helper: create a MeteringService backed by the test database.
async fn create_metering_service() -> ains_server::services::MeteringService {
    let db = common::create_test_db_and_run_migrations().await;
    ains_server::services::MeteringService::new(db)
}

/// Helper: build an OpenAI-compatible mock response with usage info.
fn openai_response(prompt: u64, completion: u64) -> Value {
    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        }
    })
}

fn anthropic_response(input: u64, output: u64) -> Value {
    json!({
        "id": "msg_mock",
        "type": "message",
        "usage": {
            "input_tokens": input,
            "output_tokens": output,
        }
    })
}

// ── MeteringService: record_usage ───────────────────────────────

#[tokio::test]
async fn test_metering_record_and_query_user_usage() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-record-{}", uid);

    let response = openai_response(100, 50);
    let result = metering
        .record_usage(uid, &tenant, channel_id, "gpt-4", "chat", &response)
        .await;

    assert!(result.is_ok(), "should record usage successfully");
    let record = result.unwrap();
    assert_eq!(record.user_id, uid);
    assert_eq!(record.tenant_id, tenant);
    assert_eq!(record.channel_id, channel_id);
    assert_eq!(record.model, "gpt-4");
    assert_eq!(record.prompt_tokens, 100);
    assert_eq!(record.completion_tokens, 50);
    assert_eq!(record.total_tokens, 150);
    assert_eq!(record.request_type, "chat");

    // Query by user — should find the record
    let user_records = metering.get_user_usage(uid, 10).await.unwrap();
    assert_eq!(
        user_records.len(),
        1,
        "should find exactly 1 usage record for this user"
    );
    assert_eq!(user_records[0].user_id, uid);
    assert_eq!(user_records[0].total_tokens, 150);
}

#[tokio::test]
async fn test_metering_anthropic_format() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-anthropic-{}", uid);

    let response = anthropic_response(200, 80);
    let result = metering
        .record_usage(uid, &tenant, channel_id, "claude-3-opus", "chat", &response)
        .await;

    assert!(result.is_ok(), "should handle Anthropic usage format");
    let record = result.unwrap();
    assert_eq!(record.prompt_tokens, 200);
    assert_eq!(record.completion_tokens, 80);
    assert_eq!(record.total_tokens, 280); // total = prompt + completion
}

#[tokio::test]
async fn test_metering_3d_accounting_dimensions() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_a = Uuid::new_v4();
    let channel_b = Uuid::new_v4();
    let tenant_a = format!("tenant-3d-a-{}", uid);
    let tenant_b = format!("tenant-3d-b-{}", uid);

    // Record usage for uid in tenant A via channel A
    metering
        .record_usage(
            uid,
            &tenant_a,
            channel_a,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();

    // Record usage for uid in tenant A via channel B (different channel)
    metering
        .record_usage(
            uid,
            &tenant_a,
            channel_b,
            "gpt-4",
            "chat",
            &openai_response(20, 10),
        )
        .await
        .unwrap();

    // Record usage for uid+1 in tenant A via channel A (different user)
    metering
        .record_usage(
            uid + 1,
            &tenant_a,
            channel_a,
            "gpt-4",
            "chat",
            &openai_response(30, 15),
        )
        .await
        .unwrap();

    // Record usage for uid in tenant B via channel A (different tenant)
    metering
        .record_usage(
            uid,
            &tenant_b,
            channel_a,
            "gpt-4",
            "chat",
            &openai_response(40, 20),
        )
        .await
        .unwrap();

    // Verify per-user aggregation
    let user_a_records = metering.get_user_usage(uid, 10).await.unwrap();
    assert_eq!(
        user_a_records.len(),
        3,
        "user {} should have 3 records (2 in tenant-a, 1 in tenant-b)",
        uid
    );

    let user_b_records = metering.get_user_usage(uid + 1, 10).await.unwrap();
    assert_eq!(
        user_b_records.len(),
        1,
        "user {} should have 1 record",
        uid + 1
    );

    // Verify per-tenant aggregation
    let tenant_a_records = metering.get_tenant_usage(&tenant_a, 10).await.unwrap();
    assert_eq!(
        tenant_a_records.len(),
        3,
        "tenant {} should have 3 records",
        tenant_a
    );

    let tenant_b_records = metering.get_tenant_usage(&tenant_b, 10).await.unwrap();
    assert_eq!(
        tenant_b_records.len(),
        1,
        "tenant {} should have 1 record",
        tenant_b
    );

    // Verify per-channel aggregation
    let channel_a_records = metering.get_channel_usage(channel_a, 10).await.unwrap();
    assert_eq!(
        channel_a_records.len(),
        3,
        "channel A should have 3 records"
    );

    let channel_b_records = metering.get_channel_usage(channel_b, 10).await.unwrap();
    assert_eq!(channel_b_records.len(), 1, "channel B should have 1 record");
}

#[tokio::test]
async fn test_metering_no_usage_field_stores_zeros() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-zero-{}", uid);

    let response = json!({"id": "no-usage", "choices": []});
    let result = metering
        .record_usage(uid, &tenant, channel_id, "gpt-4", "chat", &response)
        .await;

    assert!(
        result.is_ok(),
        "should record zero tokens when usage field is missing"
    );
    let record = result.unwrap();
    assert_eq!(record.prompt_tokens, 0);
    assert_eq!(record.completion_tokens, 0);
    assert_eq!(record.total_tokens, 0);
}

#[tokio::test]
async fn test_metering_empty_model_falls_back_to_response_model() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-fallback-{}", uid);

    let response = json!({
        "model": "gpt-4-from-response",
        "usage": {"total_tokens": 10}
    });
    let result = metering
        .record_usage(uid, &tenant, channel_id, "", "chat", &response)
        .await;

    assert!(result.is_ok());
    let record = result.unwrap();
    assert_eq!(
        record.model, "gpt-4-from-response",
        "empty model name should fall back to response's model field"
    );
}

#[tokio::test]
async fn test_metering_query_limits_respected() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-limit-{}", uid);

    // Record 5 usage entries
    for i in 0..5 {
        metering
            .record_usage(
                uid,
                &tenant,
                channel_id,
                "gpt-4",
                "chat",
                &openai_response(i * 10, i * 5),
            )
            .await
            .unwrap();
    }

    // Query with limit=2 should return at most 2 records
    let records = metering.get_user_usage(uid, 2).await.unwrap();
    assert_eq!(records.len(), 2, "limit=2 should return 2 records");

    // Query with limit=10 should return all 5 records
    let all_records = metering.get_user_usage(uid, 10).await.unwrap();
    assert_eq!(all_records.len(), 5, "limit=10 should return all 5 records");
}

// ── MeteringService: list_usage (paginated) ─────────────────

#[tokio::test]
async fn test_metering_list_usage_basic_pagination() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-list-{uid}");

    // Record 5 usage entries
    for i in 0..5 {
        metering
            .record_usage(
                uid + i,
                &tenant,
                channel_id,
                "gpt-4",
                "chat",
                &openai_response(10, 5),
            )
            .await
            .unwrap();
    }

    // Fetch first page (per_page = 2, expect 2 items)
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 2,
        tenant_id: Some(tenant.clone()),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert_eq!(resp.items.len(), 2, "page 1 should have 2 items");
    assert_eq!(resp.total, 5, "total should be 5");
    assert_eq!(resp.page, 1);
    assert_eq!(resp.per_page, 2);
    assert_eq!(resp.total_pages, 3, "ceil(5/2) = 3");

    // Fetch second page (expect 2 items)
    let params = ains_server::services::ListUsageParams {
        page: 2,
        per_page: 2,
        tenant_id: Some(tenant.clone()),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert_eq!(resp.items.len(), 2, "page 2 should have 2 items");

    // Fetch third page (expect 1 item)
    let params = ains_server::services::ListUsageParams {
        page: 3,
        per_page: 2,
        tenant_id: Some(tenant),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert_eq!(resp.items.len(), 1, "page 3 should have 1 item");
}

#[tokio::test]
async fn test_metering_list_usage_filter_by_model() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-filter-model-{uid}");

    // Record one gpt-4 and two claude entries
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "claude-3",
            "chat",
            &openai_response(20, 10),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "claude-3",
            "chat",
            &openai_response(30, 15),
        )
        .await
        .unwrap();

    // Filter by claude-3
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        model: Some("claude-3".to_string()),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert_eq!(resp.items.len(), 2, "should find 2 claude-3 records");
    assert_eq!(resp.items[0].model, "claude-3");
}

#[tokio::test]
async fn test_metering_list_usage_filter_by_date_range() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-filter-date-{uid}");

    // Record 3 entries
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(20, 10),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(30, 15),
        )
        .await
        .unwrap();

    // Filter with a date range — we can't predict exact timestamps, but we can
    // query for all records with a wide window and verify count.
    use chrono::{DateTime, Utc};
    let far_past = "2020-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let far_future = "2030-12-31T23:59:59Z".parse::<DateTime<Utc>>().unwrap();
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        date_from: Some(far_past),
        date_to: Some(far_future),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert_eq!(
        resp.items.len(),
        3,
        "should find all 3 records in wide window"
    );
}

#[tokio::test]
async fn test_metering_list_usage_empty_result() {
    let metering = create_metering_service().await;
    let uid = unique_base();
    let tenant = format!("tenant-empty-{uid}");

    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert!(resp.items.is_empty());
    assert_eq!(resp.total, 0);
    assert_eq!(resp.total_pages, 0);
}

#[tokio::test]
async fn test_metering_list_usage_filter_by_request_type() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-filter-rtype-{uid}");

    // Record 2 chat entries and 1 vision entry
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(20, 10),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4-vision",
            "vision",
            &openai_response(30, 15),
        )
        .await
        .unwrap();

    // Filter by vision
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        request_type: Some("vision".to_string()),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert_eq!(resp.items.len(), 1, "should find 1 vision record");
    assert_eq!(resp.items[0].request_type, "vision");
}

#[tokio::test]
async fn test_metering_list_usage_filter_by_model_and_request_type() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-filter-combo-{uid}");

    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "vision",
            &openai_response(20, 10),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "claude-3",
            "chat",
            &openai_response(30, 15),
        )
        .await
        .unwrap();

    // Filter by gpt-4 + vision
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        model: Some("gpt-4".to_string()),
        request_type: Some("vision".to_string()),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert_eq!(resp.items.len(), 1, "should find 1 gpt-4/vision record");
    assert_eq!(resp.items[0].model, "gpt-4");
    assert_eq!(resp.items[0].request_type, "vision");
}

// ── MeteringService: get_usage_stats ─────────────────────────

#[tokio::test]
async fn test_metering_get_usage_stats_aggregates() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-stats-{uid}");

    // Record 3 gpt-4 entries (10+20+30 = 60 prompt, 5+10+15 = 30 completion, 15+30+45 = 90 total)
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(20, 10),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(30, 15),
        )
        .await
        .unwrap();

    // Record 2 claude-3 entries (40+50 = 90 prompt, 20+25 = 45 completion)
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "claude-3",
            "chat",
            &openai_response(40, 20),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "claude-3",
            "chat",
            &openai_response(50, 25),
        )
        .await
        .unwrap();

    // Get stats for this tenant
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        ..Default::default()
    };
    let stats = metering.get_usage_stats(params).await.unwrap();

    assert_eq!(stats.total_requests, 5, "5 total requests");
    assert_eq!(stats.total_prompt_tokens, 150, "10+20+30+40+50 = 150");
    assert_eq!(stats.total_completion_tokens, 75, "5+10+15+20+25 = 75");
    assert_eq!(stats.total_tokens, 225, "15+30+45+60+75 = 225");
    assert_eq!(stats.model_breakdown.len(), 2, "2 distinct models");

    // Verify per-model breakdown
    for model_summary in &stats.model_breakdown {
        match model_summary.model.as_str() {
            "gpt-4" => {
                assert_eq!(model_summary.request_count, 3);
                assert_eq!(model_summary.total_tokens, 90);
            }
            "claude-3" => {
                assert_eq!(model_summary.request_count, 2);
                assert_eq!(model_summary.total_tokens, 135);
            }
            other => panic!("unexpected model: {other}"),
        }
    }
}

#[tokio::test]
async fn test_metering_get_usage_stats_filter_by_model() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-stats-filter-{uid}");

    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "claude-3",
            "chat",
            &openai_response(20, 10),
        )
        .await
        .unwrap();

    // Filter by claude-3
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        model: Some("claude-3".to_string()),
        ..Default::default()
    };
    let stats = metering.get_usage_stats(params).await.unwrap();
    assert_eq!(stats.total_requests, 1);
    assert_eq!(stats.model_breakdown.len(), 1);
    assert_eq!(stats.model_breakdown[0].model, "claude-3");
}

#[tokio::test]
async fn test_metering_get_usage_stats_empty_table() {
    let metering = create_metering_service().await;
    let uid = unique_base();

    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(format!("tenant-stats-empty-{uid}")),
        ..Default::default()
    };
    let stats = metering.get_usage_stats(params).await.unwrap();
    assert_eq!(stats.total_requests, 0);
    assert_eq!(stats.total_prompt_tokens, 0);
    assert_eq!(stats.total_completion_tokens, 0);
    assert_eq!(stats.total_tokens, 0);
    assert!(stats.model_breakdown.is_empty());
}

#[tokio::test]
async fn test_metering_list_usage_and_stats_consistent() {
    // Verify that `list_usage` and `get_usage_stats` return consistent counts
    // when using the same tenant filter.
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-consistent-{uid}");

    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();

    let list_params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant.clone()),
        ..Default::default()
    };
    let list_resp = metering.list_usage(list_params.clone()).await.unwrap();
    let stats = metering.get_usage_stats(list_params).await.unwrap();

    assert_eq!(
        list_resp.total, stats.total_requests,
        "list_usage.total and get_usage_stats.total_requests must match"
    );
    assert_eq!(list_resp.items.len(), 1);
    assert_eq!(stats.total_requests, 1);
}

#[tokio::test]
async fn test_metering_get_usage_stats_filter_by_date_range() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-stats-daterange-{uid}");

    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();

    // Query with a date range that should include the record
    use chrono::{DateTime, Utc};
    let far_past = "2020-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let far_future = "2030-12-31T23:59:59Z".parse::<DateTime<Utc>>().unwrap();
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        date_from: Some(far_past),
        date_to: Some(far_future),
        ..Default::default()
    };
    let stats = metering.get_usage_stats(params).await.unwrap();
    assert_eq!(stats.total_requests, 1);
    assert_eq!(stats.total_tokens, 15);
}

// ── MeteringService: get_usage_stats date range excludes all ─────

#[tokio::test]
async fn test_metering_get_usage_stats_date_range_excludes_all() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-stats-excludeall-{uid}");

    // Record 2 entries
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(20, 10),
        )
        .await
        .unwrap();

    // Query with a date range far in the future — should match zero records.
    use chrono::{DateTime, Utc};
    let far_future_start = "2030-06-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let far_future_end = "2030-06-30T23:59:59Z".parse::<DateTime<Utc>>().unwrap();
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        date_from: Some(far_future_start),
        date_to: Some(far_future_end),
        ..Default::default()
    };
    let stats = metering.get_usage_stats(params).await.unwrap();
    assert_eq!(stats.total_requests, 0, "no records in future range");
    assert_eq!(stats.total_prompt_tokens, 0);
    assert_eq!(stats.total_completion_tokens, 0);
    assert_eq!(stats.total_tokens, 0);
    assert!(stats.model_breakdown.is_empty());
}

// ── MeteringService: list_usage filter by channel_id ─────────────

#[tokio::test]
async fn test_metering_list_usage_filter_by_channel_id() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_a = Uuid::new_v4();
    let channel_b = Uuid::new_v4();
    let tenant = format!("tenant-filter-ch-{uid}");

    // Record 3 entries on channel_a, 2 on channel_b
    for _ in 0..3 {
        metering
            .record_usage(
                uid,
                &tenant,
                channel_a,
                "gpt-4",
                "chat",
                &openai_response(10, 5),
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        metering
            .record_usage(
                uid,
                &tenant,
                channel_b,
                "gpt-4",
                "chat",
                &openai_response(20, 10),
            )
            .await
            .unwrap();
    }

    // Filter by channel_a
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        channel_id: Some(channel_a),
        ..Default::default()
    };
    let resp = metering.list_usage(params).await.unwrap();
    assert_eq!(resp.items.len(), 3, "should find 3 records for channel_a");
    assert_eq!(resp.total, 3);
    for item in &resp.items {
        assert_eq!(
            item.channel_id, channel_a,
            "all records should belong to channel_a"
        );
    }
}

// ── MeteringService: record_usage id=0 sentinel for missing usage field

#[tokio::test]
async fn test_metering_record_usage_no_usage_field_id_sentinel() {
    let uid = unique_base();
    let metering = create_metering_service().await;
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-sentinel-{uid}");

    // A response body without a "usage" field (e.g. streaming SSE event).
    let response_no_usage = json!({"id": "chatcmpl-no-usage", "object": "chat.completion.chunk"});
    let result = metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &response_no_usage,
        )
        .await
        .unwrap();

    // The sentinel value id=0 indicates the record was NOT persisted.
    assert_eq!(
        result.id, 0,
        "missing usage field → id=0 sentinel (not persisted)"
    );

    // Verify it did NOT appear in list_usage.
    let params = ains_server::services::ListUsageParams {
        page: 1,
        per_page: 10,
        tenant_id: Some(tenant),
        ..Default::default()
    };
    let list = metering.list_usage(params).await.unwrap();
    assert!(
        list.items.is_empty(),
        "no-usage record should not be persisted to DB"
    );
}

// ── HTTP-level integration: admin tenant-scoping ─────────────────────
//
// Verifies that the `/api/usage` and `/api/usage/stats` endpoints enforce
// tenant isolation: admin users can only see their own tenant's usage,
// even if they explicitly request a different tenant_id in the query.

#[cfg(not(feature = "ains-salvo"))]
#[tokio::test]
async fn test_metering_http_admin_tenant_isolation() {
    let (app, state) = common::axum::create_app_and_state().await;
    let metering = ains_server::services::MeteringService::new(state.db.clone());

    let channel_id = Uuid::new_v4();
    let uid = unique_base();
    let tenant_b = format!("tenant-iso-b-{uid}");
    // A per-test-unique model name isolates our records from the shared
    // "default" tenant, which is polluted by other tests running in parallel.
    let iso_model = format!("iso-model-{uid}");

    // Create a second tenant via system user
    let sys_email = common::unique_email("metering_iso_sys");
    let sys_token = common::axum::create_system_and_login(&app, &sys_email).await;

    // Register tenant B (system-only endpoint — must carry the system token)
    let post_resp = common::axum::send_request(
        &app,
        Method::POST,
        "/api/tenants",
        vec![
            ("content-type", "application/json"),
            ("authorization", &format!("Bearer {sys_token}")),
        ],
        Body::from(serde_json::json!({"name": &tenant_b}).to_string()),
    )
    .await;
    assert_eq!(post_resp.status(), StatusCode::OK);
    // Verify the response includes a non-empty tenant ID.
    // Note: the create() service generates a UUID, not the input name.
    let tenant_b_id = common::axum::body_to_json(post_resp).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!tenant_b_id.is_empty(), "tenant id should be non-empty");

    // Record usage under the unique model: 1 entry in the default tenant
    // (where the admin lives) and 1 entry in tenant_b (cross-tenant). The
    // admin must see the default entry but never the tenant_b entry.
    metering
        .record_usage(
            uid,
            "default",
            channel_id,
            &iso_model,
            "chat",
            &openai_response(10, 5),
        )
        .await
        .unwrap();
    metering
        .record_usage(
            uid,
            &tenant_b,
            channel_id,
            &iso_model,
            "chat",
            &openai_response(20, 10),
        )
        .await
        .unwrap();

    // Create an admin in the default tenant.
    let admin_email = common::unique_email("metering_iso_admin");
    let admin_token = common::axum::create_admin_and_login(&app, &admin_email).await;

    // Admin tries to list usage with tenant_id=tenant_b (cross-tenant). The
    // handler must force the query back to the admin's own tenant ("default"),
    // so only the default record for `iso_model` is visible — never tenant_b's.
    let (status, body) = get(
        &app,
        &format!("/api/usage?tenant_id={tenant_b}&model={iso_model}&page=1&per_page=10"),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "admin forced to own tenant should see only the default record for this model"
    );
    for item in items {
        assert_eq!(
            item["tenant_id"], "default",
            "admin must never see a tenant_b record, even when requesting tenant_id=tenant_b"
        );
    }

    // System can target tenant_b explicitly and see exactly its record.
    let (status, body) = get(
        &app,
        &format!("/api/usage?tenant_id={tenant_b}&model={iso_model}&page=1&per_page=10"),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "system should see tenant_b usage");
    assert_eq!(items[0]["tenant_id"], tenant_b);

    // Stats endpoint: admin scoped to own tenant. Requesting tenant_b must
    // still only aggregate the default record (15 tokens), not tenant_b's 30.
    let (status, body) = get(
        &app,
        &format!("/api/usage/stats?tenant_id={tenant_b}&model={iso_model}"),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total_requests"], 1,
        "admin stats scoped to own tenant"
    );
    assert_eq!(
        body["total_tokens"], 15,
        "admin stats must exclude tenant_b tokens"
    );

    // System stats can see tenant_b's record (30 tokens).
    let (status, body) = get(
        &app,
        &format!("/api/usage/stats?tenant_id={tenant_b}&model={iso_model}"),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total_requests"], 1,
        "system should see tenant_b stats"
    );
    assert_eq!(
        body["total_tokens"], 30,
        "system stats reflect tenant_b tokens"
    );
}

// ── HTTP-level integration: date_to end-of-day via handler ──────────
//
// Verifies that a bare date string (e.g. "2026-07-31") for `date_to`
// is expanded to end-of-day (23:59:59.999999) by the handler, so that
// records created during the final day are included in the range.

#[cfg(not(feature = "ains-salvo"))]
#[tokio::test]
async fn test_metering_http_date_to_end_of_day() {
    let (app, state) = common::axum::create_app_and_state().await;
    let metering = ains_server::services::MeteringService::new(state.db.clone());

    let uid = unique_base();
    let channel_id = Uuid::new_v4();
    let tenant = format!("tenant-date-eod-{uid}");

    // Record usage now (it will have today's timestamp)
    metering
        .record_usage(
            uid,
            &tenant,
            channel_id,
            "gpt-4",
            "chat",
            &openai_response(30, 15),
        )
        .await
        .unwrap();

    let sys_email = common::unique_email("metering_date_eod_sys");
    let sys_token = common::axum::create_system_and_login(&app, &sys_email).await;

    // Use today's date (bare date, no time) for date_to — handler expands to end-of-day
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    let (status, body) = get(
        &app,
        &format!(
            "/api/usage/stats?tenant_id={}&date_from={}&date_to={}",
            tenant, yesterday, today
        ),
        Some(&sys_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total_requests"], 1,
        "bare date_to should be expanded to end-of-day, including today's record"
    );
}

// ── Helpers reused from the top of this file (get helper for HTTP tests) ──

#[cfg(not(feature = "ains-salvo"))]
async fn get(
    app: &ains_axum::Router,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = ains_axum::Request::builder().method(Method::GET).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {}", t));
    }
    let request = req.body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, body)
}
