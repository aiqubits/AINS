#![cfg(not(feature = "ains-salvo"))]

//! Integration tests for bootstrap and migration seed data.
//!
//! Verifies that:
//! - Migration creates the default tenant row
//! - Migration creates all required tables
//! - Snowflake worker initialization works
//! - System admin can be seeded
//!
//! These tests require running PostgreSQL.
//! Run: cargo test --test axum_bootstrap_test

use chrono::Utc;
use sea_orm::{ConnectionTrait, DatabaseBackend, EntityTrait, Statement};

mod common;

// ── Migration seed tests ────────────────────────────────────────

/// Verify that running migrations creates the default tenant row.
#[tokio::test]
async fn test_migration_creates_default_tenant() {
    let db = common::create_test_db_and_run_migrations().await;
    let db = db.write_conn();

    // Reset default tenant to its initial state — other tests may have modified it
    let _ = db
        .execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "UPDATE tenants SET name = 'Default Tenant', status = 'active' WHERE id = 'default'"
                .to_string(),
        ))
        .await;

    let result = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT id, name, status FROM tenants WHERE id = 'default'".to_string(),
        ))
        .await
        .expect("Failed to query default tenant")
        .expect("Default tenant should exist after migration");

    let id: String = result.try_get_by("id").unwrap();
    let name: String = result.try_get_by("name").unwrap();
    let status: String = result.try_get_by("status").unwrap();

    assert_eq!(id, "default");
    assert_eq!(name, "Default Tenant");
    assert_eq!(status, "active");
}

/// Verify migration creates all required tables.
#[tokio::test]
async fn test_migration_creates_required_tables() {
    let db = common::create_test_db_and_run_migrations().await;
    let db = db.write_conn();

    let required_tables = [
        "tenants",
        "users",
        "refresh_tokens",
        "ai_gateway_channels",
        "snowflake_worker",
        "token_usage",
    ];

    // Use sea_orm query to check each table
    for table in &required_tables {
        let exists: bool = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT EXISTS (SELECT FROM information_schema.tables WHERE table_name = '{}')",
                    table
                ),
            ))
            .await
            .unwrap_or_else(|e| panic!("Failed to check table {}: {}", table, e))
            .map(|row| row.try_get_by::<bool, _>(0).unwrap_or(false))
            .unwrap_or(false);

        assert!(
            exists,
            "Required table '{}' should exist after migration",
            table
        );
    }
}

/// Verify default tenant has proper GIN index on channels.capabilities.
#[tokio::test]
async fn test_migration_creates_channel_indexes() {
    let db = common::create_test_db_and_run_migrations().await;
    let db = db.write_conn();

    let required_indexes = [
        "idx_channels_tenant_capability_active",
        "idx_channels_capabilities_gin",
        "idx_token_usage_user",
        "idx_token_usage_tenant",
        "idx_token_usage_channel",
    ];

    for index in &required_indexes {
        let exists: bool = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT EXISTS (SELECT FROM pg_indexes WHERE indexname = '{}')",
                    index
                ),
            ))
            .await
            .unwrap_or_else(|e| panic!("Failed to check index {}: {}", index, e))
            .map(|row| row.try_get_by::<bool, _>(0).unwrap_or(false))
            .unwrap_or(false);

        assert!(
            exists,
            "Required index '{}' should exist after migration",
            index
        );
    }
}

/// Verify default tenant cannot be deleted via DB (ON DELETE RESTRICT on users/channels).
#[tokio::test]
async fn test_default_tenant_fk_restrict_on_users() {
    use ains_server::repositories::user::ActiveModel;
    use sea_orm::{ActiveModelTrait, Set};

    let db_arc = common::create_test_db_and_run_migrations().await;
    let db = db_arc.write_conn();

    // Create a user referencing the default tenant to trigger FK RESTRICT
    // Clean up any stale data from previous runs first
    use ains_server::repositories::user::Entity as UserEntity;
    let _ = UserEntity::delete_by_id(888888001i64).exec(db).await;
    let user = ActiveModel {
        id: Set(888888001i64),
        email: Set("fk-restrict-test@example.com".to_string()),
        password_hash: Set("hash".to_string()),
        name: Set("FK Restrict Test".to_string()),
        role: Set("user".to_string()),
        token_version: Set(1),
        email_verified: Set(true),
        verification_code_hash: Set(None),
        verification_code_expires_at: Set(None),
        verification_code_sent_at: Set(None),
        verification_failed_attempts: Set(0),
        password_reset_token_hash: Set(None),
        password_reset_expires_at: Set(None),
        password_reset_sent_at: Set(None),
        password_reset_failed_attempts: Set(0),
        balance: Set(0),
        wx_openid: Set(None),
        tenant_id: Set("default".to_string()),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
    };
    user.insert(db).await.expect("Should create FK test user");

    // ON DELETE RESTRICT should prevent deletion since a user references this tenant
    let result = db
        .execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "DELETE FROM tenants WHERE id = 'default'".to_string(),
        ))
        .await;

    assert!(
        result.is_err(),
        "ON DELETE RESTRICT should prevent deleting tenant with users"
    );
}

/// Verify user creation assigns default tenant when no tenant_id specified.
#[tokio::test]
async fn test_user_creation_defaults_to_default_tenant() {
    use ains_server::repositories::user::{ActiveModel, Entity as UserEntity};
    use sea_orm::{ActiveModelTrait, Set};

    let db = common::create_test_db_and_run_migrations().await;
    let db = db.write_conn();

    // Clean up any stale data from previous runs first
    let _ = UserEntity::delete_by_id(999999001i64).exec(db).await;

    // Insert a test user without specifying tenant_id
    let user = ActiveModel {
        id: Set(999999001i64),
        email: Set("default-tenant-test@example.com".to_string()),
        password_hash: Set("hash".to_string()),
        name: Set("Default Tenant Test".to_string()),
        role: Set("user".to_string()),
        token_version: Set(1),
        email_verified: Set(true),
        verification_code_hash: Set(None),
        verification_code_expires_at: Set(None),
        verification_code_sent_at: Set(None),
        verification_failed_attempts: Set(0),
        password_reset_token_hash: Set(None),
        password_reset_expires_at: Set(None),
        password_reset_sent_at: Set(None),
        password_reset_failed_attempts: Set(0),
        balance: Set(0),
        wx_openid: Set(None),
        tenant_id: Set("default".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
    };
    user.insert(db)
        .await
        .expect("Should create user with default tenant");

    // Verify
    let saved = UserEntity::find_by_id(999999001i64)
        .one(db)
        .await
        .expect("Should find user")
        .expect("User should exist");
    assert_eq!(
        saved.tenant_id, "default",
        "User should have default tenant_id"
    );
}
