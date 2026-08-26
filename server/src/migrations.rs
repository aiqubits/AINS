use anyhow::{Context, Result};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};

const MIGRATIONS: &[(&str, &str)] = &[("001_init", include_str!("../migrations/001_init.sql"))];

// "AINS_MIG" encoded as a positive i64. PostgreSQL advisory locks are
// database-scoped, so every application instance contends on the same key.
#[doc(hidden)]
pub const MIGRATION_ADVISORY_LOCK_KEY: i64 = 0x4149_4E53_5F4D_4947;

/// Check if a database error is an expected "already exists" error for
/// idempotent migration statements (CREATE TABLE IF NOT EXISTS, CREATE INDEX IF NOT EXISTS).
///
/// Matches PostgreSQL's duplicate_table (42P07) and duplicate_object (42710)
/// errors, which are not covered by sea-orm's SqlErr enum.
fn is_duplicate_table_or_index_error(err: &DbErr) -> bool {
    // String matching for errors not covered by SqlErr:
    // - 42P07: duplicate_table (CREATE TABLE IF NOT EXISTS safety net)
    // - 42710: duplicate_object (CREATE INDEX IF NOT EXISTS, CREATE TYPE IF NOT EXISTS safety net)
    let msg = err.to_string().to_lowercase();

    // Check for PostgreSQL error codes
    if msg.contains("42p07") || msg.contains("42710") {
        return true;
    }

    // Check for "already exists" messages for various object types
    if msg.contains("already exists") {
        return msg.contains("relation")
            || msg.contains("type")
            || msg.contains("index")
            || msg.contains("table");
    }

    false
}

/// Run database migrations.
///
/// # Limitations
///
/// **SQL splitting**: Statements are split by `;` using simple string splitting.
/// This means SQL string literals containing semicolons (e.g., `INSERT INTO t
/// VALUES ('hello;world')`) will be incorrectly split. Current migrations are
/// simple enough that this is not triggered, but future migrations must avoid
/// semicolons inside string literals, or the splitting logic should be upgraded
/// to use a proper SQL parser.
///
/// **Greenfield-only schema policy**: There is no `_migrations` table because
/// this project currently supports new deployments only. `001_init.sql` is the
/// canonical complete schema and is re-executed on startup using idempotent
/// statements (`IF NOT EXISTS`). Schema changes must be made directly in that
/// file; incremental compatibility migrations are intentionally out of scope
/// until retained production databases are explicitly supported.
pub async fn run_migrations(db: &DatabaseConnection) -> Result<()> {
    tracing::info!("Running database migrations...");

    // `IF NOT EXISTS` does not make concurrent PostgreSQL DDL race-free: two
    // fresh application instances can both pass the existence check and then
    // collide while creating the relation's pg_type row. Keep the lock and all
    // schema statements on one transaction/connection so startup is serialized
    // across processes and the complete canonical schema is committed atomically.
    let migration = db
        .begin()
        .await
        .context("Failed to begin migration transaction")?;
    migration
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT pg_advisory_xact_lock({MIGRATION_ADVISORY_LOCK_KEY})"),
        ))
        .await
        .context("Failed to acquire migration advisory lock")?;

    // Greenfield-only: every schema field belongs directly in 001_init.sql's
    // CREATE TABLE statements; do not add ALTER TABLE compatibility migrations.
    for &(name, sql) in MIGRATIONS {
        tracing::info!("Running migration: {}", name);

        // Split by semicolon and execute each statement in its own savepoint
        // to prevent one failing statement from aborting the entire transaction
        for statement in sql.split(';') {
            let trimmed = statement.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Use a savepoint for each statement so that failures don't abort
            // the entire migration transaction (PostgreSQL requires this)
            let savepoint = migration
                .begin()
                .await
                .context("Failed to begin savepoint")?;

            match savepoint
                .execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    trimmed.to_string(),
                ))
                .await
            {
                Ok(_) => {
                    savepoint
                        .commit()
                        .await
                        .context("Failed to commit savepoint")?;
                }
                Err(e) => {
                    savepoint
                        .rollback()
                        .await
                        .context("Failed to rollback savepoint")?;

                    let is_expected_error = is_duplicate_table_or_index_error(&e);

                    if is_expected_error {
                        tracing::warn!("Migration statement skipped (already exists): {}", trimmed);
                    } else {
                        return Err(e).context(format!("Failed to execute migration: {}", name));
                    }
                }
            }
        }

        tracing::info!("Migration completed: {}", name);
    }

    migration
        .commit()
        .await
        .context("Failed to commit migration transaction")?;

    tracing::info!("All migrations completed successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::MIGRATIONS;

    fn mask_sql_comments_and_quoted_content(sql: &str) -> String {
        #[derive(Clone, Copy)]
        enum ScanState {
            Normal,
            SingleQuoted,
            DoubleQuoted,
            LineComment,
            BlockComment(usize),
        }

        let chars = sql.chars().collect::<Vec<_>>();
        let mut output = String::with_capacity(sql.len());
        let mut state = ScanState::Normal;
        let mut index = 0;

        while index < chars.len() {
            let current = chars[index];
            let next = chars.get(index + 1).copied();

            match state {
                ScanState::Normal => match (current, next) {
                    ('-', Some('-')) => {
                        output.push(' ');
                        index += 2;
                        state = ScanState::LineComment;
                    }
                    ('/', Some('*')) => {
                        output.push(' ');
                        index += 2;
                        state = ScanState::BlockComment(1);
                    }
                    ('\'', _) => {
                        output.push(current);
                        index += 1;
                        state = ScanState::SingleQuoted;
                    }
                    ('"', _) => {
                        output.push(current);
                        index += 1;
                        state = ScanState::DoubleQuoted;
                    }
                    _ => {
                        output.push(current);
                        index += 1;
                    }
                },
                ScanState::SingleQuoted => {
                    if current == '\'' {
                        if next == Some('\'') {
                            output.push_str("  ");
                            index += 2;
                        } else {
                            output.push(current);
                            index += 1;
                            state = ScanState::Normal;
                        }
                    } else {
                        output.push(if current == '\n' { '\n' } else { ' ' });
                        index += 1;
                    }
                }
                ScanState::DoubleQuoted => {
                    if current == '"' {
                        if next == Some('"') {
                            output.push_str("  ");
                            index += 2;
                        } else {
                            output.push(current);
                            index += 1;
                            state = ScanState::Normal;
                        }
                    } else {
                        output.push(if current == '\n' { '\n' } else { ' ' });
                        index += 1;
                    }
                }
                ScanState::LineComment => {
                    if current == '\n' {
                        output.push('\n');
                        state = ScanState::Normal;
                    }
                    index += 1;
                }
                ScanState::BlockComment(depth) => match (current, next) {
                    ('/', Some('*')) => {
                        index += 2;
                        state = ScanState::BlockComment(depth + 1);
                    }
                    ('*', Some('/')) => {
                        index += 2;
                        state = if depth == 1 {
                            ScanState::Normal
                        } else {
                            ScanState::BlockComment(depth - 1)
                        };
                    }
                    _ => {
                        if current == '\n' {
                            output.push('\n');
                        }
                        index += 1;
                    }
                },
            }
        }

        output
    }

    fn canonical_schema_policy_violations(sql: &str) -> Vec<String> {
        let executable_sql = mask_sql_comments_and_quoted_content(sql).to_ascii_uppercase();
        let mut violations = Vec::new();

        for statement in executable_sql.split(';') {
            let normalized = statement.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                continue;
            }

            if normalized == "ALTER TABLE" || normalized.starts_with("ALTER TABLE ") {
                violations.push(format!(
                    "001_init.sql must define the complete greenfield schema with CREATE statements: {normalized}"
                ));
            }
            if normalized.starts_with("CREATE TABLE ")
                && !normalized.starts_with("CREATE TABLE IF NOT EXISTS ")
            {
                violations.push(format!(
                    "startup-replayed table statement must be idempotent: {normalized}"
                ));
            }
            if (normalized.starts_with("CREATE INDEX ")
                || normalized.starts_with("CREATE UNIQUE INDEX "))
                && !normalized.contains(" INDEX IF NOT EXISTS ")
            {
                violations.push(format!(
                    "startup-replayed index statement must be idempotent: {normalized}"
                ));
            }
            if (normalized == "INSERT INTO" || normalized.starts_with("INSERT INTO "))
                && !normalized.contains(" ON CONFLICT ")
            {
                violations.push(format!(
                    "startup-replayed seed INSERT must use ON CONFLICT: {normalized}"
                ));
            }
        }

        violations
    }

    #[test]
    fn test_migrations_list() {
        let migrations = MIGRATIONS;

        assert_eq!(migrations.len(), 1);
        assert!(migrations[0].1.contains("CREATE TABLE IF NOT EXISTS users"));
        assert!(migrations[0].1.contains("token_version"));
        assert!(
            migrations[0]
                .1
                .contains("CREATE TABLE IF NOT EXISTS tenants")
        );
        assert!(migrations[0].1.contains("ai_gateway_channels"));
        assert!(migrations[0].1.contains("token_usage"));
        assert!(migrations[0].1.contains("idx_token_usage_model"));
        assert!(migrations[0].1.contains("CREATE TABLE IF NOT EXISTS plans"));
        assert!(
            migrations[0]
                .1
                .contains("price BIGINT NOT NULL CHECK (price >= 0)")
        );
        assert!(migrations[0].1.contains(
            "purchase_limit INTEGER CHECK (purchase_limit IS NULL OR purchase_limit > 0)"
        ));
        assert!(
            migrations[0]
                .1
                .contains("CREATE TABLE IF NOT EXISTS user_plans")
        );
        assert!(
            migrations[0]
                .1
                .contains("CREATE TABLE IF NOT EXISTS payment_orders")
        );
        assert!(migrations[0].1.contains("idx_plans_tenant_status"));
        assert!(migrations[0].1.contains("idx_user_plans_user"));
        assert!(migrations[0].1.contains(
            "CREATE INDEX IF NOT EXISTS idx_user_plans_purchase_count\n    ON user_plans(user_id, plan_id)\n    WHERE source = 'purchase'"
        ));
        assert!(migrations[0].1.contains("idx_payment_orders_tenant"));
    }

    #[test]
    fn migration_directory_contains_only_the_canonical_init_sql() {
        let migration_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut sql_files = fs::read_dir(&migration_dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", migration_dir.display()))
            .map(|entry| {
                entry
                    .expect("failed to read migration directory entry")
                    .path()
            })
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
            })
            .map(|path| {
                path.file_name()
                    .expect("migration SQL file must have a name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        sql_files.sort();

        assert_eq!(
            sql_files,
            ["001_init.sql"],
            "greenfield-only policy requires all schema changes to stay in 001_init.sql"
        );
    }

    #[test]
    fn canonical_schema_follows_greenfield_policy() {
        let violations = canonical_schema_policy_violations(MIGRATIONS[0].1);
        assert!(
            violations.is_empty(),
            "canonical schema policy violations:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn canonical_schema_policy_rejects_multiline_alter_table() {
        let violations =
            canonical_schema_policy_violations("ALTER\nTABLE users ADD COLUMN display_name TEXT;");

        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("ALTER TABLE"));
    }

    #[test]
    fn canonical_schema_policy_rejects_alter_table_split_by_block_comment() {
        let violations = canonical_schema_policy_violations(
            "ALTER/* compatibility path */TABLE users ADD COLUMN display_name TEXT;",
        );

        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("ALTER TABLE"));
    }

    #[test]
    fn canonical_schema_policy_rejects_non_idempotent_create_statements() {
        let violations = canonical_schema_policy_violations(
            "CREATE TABLE books (id BIGINT);\n\
             CREATE UNIQUE INDEX idx_books_id ON books(id);",
        );

        assert_eq!(violations.len(), 2);
        assert!(violations[0].contains("CREATE TABLE BOOKS"));
        assert!(violations[1].contains("CREATE UNIQUE INDEX IDX_BOOKS_ID"));
    }

    #[test]
    fn canonical_schema_policy_ignores_comments_and_quoted_text() {
        let violations = canonical_schema_policy_violations(
            "-- ALTER TABLE is documented here only\n\
             /* outer comment /* CREATE TABLE books (id BIGINT); */ still comment */\n\
             CREATE TABLE IF NOT EXISTS \"books--archive\" (id BIGINT);\n\
             CREATE INDEX IF NOT EXISTS idx_books_id ON books(id);\n\
             INSERT INTO notes (id, body) VALUES (1, 'ALTER ''TABLE -- example')\n\
                 ON CONFLICT (id) DO NOTHING;",
        );

        assert!(violations.is_empty(), "{}", violations.join("\n"));
    }

    #[test]
    fn canonical_schema_policy_keeps_quoted_dashes_from_hiding_following_statements() {
        let violations = canonical_schema_policy_violations(
            "INSERT INTO notes (id, body) VALUES (1, 'x--y')\n\
                 ON CONFLICT (id) DO NOTHING;\n\
             ALTER TABLE users ADD COLUMN display_name TEXT;",
        );

        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("ALTER TABLE"));
    }

    #[test]
    fn canonical_schema_policy_rejects_non_idempotent_seed_insert() {
        let violations = canonical_schema_policy_violations(
            "INSERT INTO tenants (id, name) VALUES ('default', 'mentions ON CONFLICT only');",
        );

        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("seed INSERT"));
    }
}
