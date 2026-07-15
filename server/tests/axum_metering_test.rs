#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for Token Metering (AINS_SERVER_PLAN §0.13).
//!
//! Tests MeteringService directly against a real PostgreSQL database,
//! verifying the 3D accounting dimensions (user_id + tenant_id + channel_id).
//!
//! These tests require running PostgreSQL and Redis instances.
//! Run: cargo test --test axum_metering_test

use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
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
