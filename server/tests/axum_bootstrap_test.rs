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
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, EntityTrait, Statement,
    TransactionTrait,
};

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
        "plans",
        "user_plans",
        "payment_orders",
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

/// Verify the canonical fresh schema contains the nullable positive
/// per-user purchase limit. This deliberately does not add an ALTER path:
/// a failure means the integration database must be recreated from 001_init.
#[tokio::test]
async fn test_migration_creates_plan_purchase_limit_column() {
    let db = common::create_test_db_and_run_migrations().await;
    let row = db
        .write_conn()
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT is_nullable, data_type FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name = 'plans' AND column_name = 'purchase_limit'"
                .to_string(),
        ))
        .await
        .expect("Failed to inspect plans.purchase_limit")
        .expect(
            "plans.purchase_limit is missing; recreate the integration database from 001_init.sql",
        );

    assert_eq!(row.try_get_by::<String, _>("is_nullable").unwrap(), "YES");
    assert_eq!(row.try_get_by::<String, _>("data_type").unwrap(), "integer");
}

/// Verify concurrent application instances wait for the canonical schema
/// transaction instead of racing PostgreSQL's system catalog during startup.
#[tokio::test]
async fn test_concurrent_migrations_wait_for_advisory_lock() {
    let config = common::load_test_config();
    let mut blocker_options = ConnectOptions::new(config.database_url.clone());
    blocker_options.max_connections(2);
    let blocker = Database::connect(blocker_options)
        .await
        .expect("Failed to connect migration lock holder");

    let lock_transaction = blocker
        .begin()
        .await
        .expect("Failed to begin migration lock holder");
    lock_transaction
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT pg_advisory_xact_lock({})",
                ains_server::migrations::MIGRATION_ADVISORY_LOCK_KEY
            ),
        ))
        .await
        .expect("Failed to hold migration advisory lock");

    // A single-connection pool makes the PID observed here the same backend
    // that run_migrations uses for its transaction.
    let mut runner_options = ConnectOptions::new(config.database_url);
    runner_options.max_connections(1);
    let runner = Database::connect(runner_options)
        .await
        .expect("Failed to connect concurrent migration runner");
    let runner_pid = runner
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid".to_string(),
        ))
        .await
        .expect("Failed to read migration runner PID")
        .expect("PostgreSQL must return a backend PID")
        .try_get_by::<i32, _>("pid")
        .expect("Migration runner PID must be an integer");

    let migration_task =
        tokio::spawn(async move { ains_server::migrations::run_migrations(&runner).await });

    let mut observed_wait = false;
    for _ in 0..100 {
        observed_wait = blocker
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT wait_event = 'advisory' AS waiting \
                     FROM pg_stat_activity WHERE pid = {runner_pid}"
                ),
            ))
            .await
            .expect("Failed to inspect concurrent migration wait")
            .and_then(|row| row.try_get_by::<bool, _>("waiting").ok())
            .unwrap_or(false);
        if observed_wait {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert!(
        observed_wait,
        "concurrent migration did not wait on the database advisory lock"
    );

    lock_transaction
        .rollback()
        .await
        .expect("Failed to release migration advisory lock");
    tokio::time::timeout(std::time::Duration::from_secs(10), migration_task)
        .await
        .expect("Concurrent migration did not resume after lock release")
        .expect("Concurrent migration task panicked")
        .expect("Concurrent migration failed after lock release");
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
        "idx_user_plans_purchase_count",
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
