use async_trait::async_trait;
use parking_lot::Mutex;
use rand::Rng;
use sea_orm::{
    AccessMode, ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DatabaseTransaction,
    DbBackend, DbErr, ExecResult, IsolationLevel, QueryResult, RuntimeErr, Statement,
    TransactionError, TransactionTrait,
};
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::utils::config::{DatabaseConfig, DatabaseReadConfig, DatabaseRoutingConfig};

/// Read replica selection strategy
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReadStrategy {
    RoundRobin = 0,
    Random = 1,
    Weighted = 2,
}

/// A single read replica connection
struct ReadReplica {
    conn: DatabaseConnection,
    /// Original index in the `database_read_urls` config array.
    /// Used to correctly map configured weights even when some replicas
    /// fail to connect (preventing weight-to-replica misalignment).
    #[allow(dead_code)]
    original_index: usize,
    weight: u32,
}

/// Tracks which read replicas are temporarily marked as down
struct HealthState {
    down_until: Vec<Option<Instant>>,
}

/// Application-level router that implements SeaORM's `ConnectionTrait` and
/// `TransactionTrait` to provide transparent read-write splitting.
///
/// - `execute` / `execute_unprepared` → always routed to the write database
/// - `query_one` / `query_all` → routed to read replicas (with retry, circuit
///   breaker, and fallback to write)
/// - `SELECT ... FOR UPDATE / FOR SHARE` → forced to the write database
///   (detected at the SQL template level)
/// - `begin` / `transaction` → always on the write database
///
/// When no read replicas are configured, `AutoRouter` transparently degenerates
/// into a single-database pass-through.
pub struct AutoRouter {
    write: DatabaseConnection,
    reads: Vec<ReadReplica>,
    strategy: ReadStrategy,
    rr_counter: AtomicUsize,
    health: Mutex<HealthState>,
    circuit_break: Duration,
    /// Extra retry attempts after all read replicas have been tried once.
    /// Each extra attempt tries any non-circuit-broken replica (including
    /// those that previously failed but whose circuit breaker has expired).
    retry_attempts: usize,
    fallback_to_write: bool,
}

/// Overall timeout in seconds for each read replica connection attempt.
/// This is a safety net on top of the per-connection connect_timeout
/// to prevent startup hangs when a replica host is completely unreachable
/// (e.g., firewall silently dropping SYN packets).
const CONNECT_TIMEOUT_SECS: u64 = 30;

impl AutoRouter {
    /// Create a multi-database router (one writer + N readers).
    pub async fn new(
        write_url: &str,
        read_urls: &[String],
        write_config: &DatabaseConfig,
        read_config: &DatabaseReadConfig,
        routing_config: &DatabaseRoutingConfig,
    ) -> Result<Arc<Self>, DbErr> {
        // 1. Connect to the write database
        let write = connect_db(write_url, write_config).await?;

        // Fail-fast when read_weights length doesn't match — a silent weight
        // fallback masks configuration mistakes that are hard to debug.
        if !routing_config.read_weights.is_empty()
            && routing_config.read_weights.len() != read_urls.len()
        {
            return Err(DbErr::Custom(format!(
                "read_weights length ({}) does not match database_read_urls count ({}); each read replica must have a corresponding weight, or remove read_weights entirely to default all replicas to weight 1",
                routing_config.read_weights.len(),
                read_urls.len(),
            )));
        }

        // 2. Connect to read replicas in parallel, tracking original indices
        let urls_with_idx: Vec<(usize, String)> = read_urls
            .iter()
            .enumerate()
            .map(|(i, u)| (i, u.clone()))
            .collect();
        let mut reads: Vec<ReadReplica> = Vec::with_capacity(urls_with_idx.len());
        let handles: Vec<_> = urls_with_idx
            .into_iter()
            .map(|(i, url)| {
                let cfg = read_config.clone();
                tokio::spawn(async move {
                    match tokio::time::timeout(
                        Duration::from_secs(CONNECT_TIMEOUT_SECS),
                        connect_db_read(&url, &cfg),
                    )
                    .await
                    {
                        Ok(result) => (i, result),
                        Err(_) => (
                            i,
                            Err(DbErr::Custom("read replica connection timed out".into())),
                        ),
                    }
                })
            })
            .collect();

        for handle in handles {
            match handle.await {
                Ok((i, Ok(conn))) => {
                    // Compute weight directly using original_index.
                    // This eliminates the need for a secondary weight-application loop.
                    let weight = if !routing_config.read_weights.is_empty() {
                        routing_config
                            .read_weights
                            .get(i)
                            .copied()
                            .unwrap_or(1)
                            .max(1)
                    } else {
                        1
                    };
                    reads.push(ReadReplica {
                        conn,
                        original_index: i,
                        weight,
                    });
                }
                Ok((i, Err(e))) => {
                    tracing::warn!("Read replica {} failed to connect: {}", i, e);
                }
                Err(e) => {
                    tracing::error!("Read replica connection task panicked: {:?}", e);
                }
            }
        }

        // 3. If no read replicas connected, fall back to single-db mode
        if reads.is_empty() {
            tracing::warn!("No read replicas connected — running in single-database mode");
            return Ok(Arc::new(Self::new_internal(
                write,
                Vec::new(),
                routing_config,
            )));
        }

        tracing::info!(
            "AutoRouter initialized with {} read replica(s)",
            reads.len()
        );

        Ok(Arc::new(Self::new_internal(write, reads, routing_config)))
    }

    /// Create a single-database router (no read-write splitting).
    pub fn single(write: DatabaseConnection) -> Arc<Self> {
        Arc::new(Self {
            write,
            reads: vec![],
            strategy: ReadStrategy::RoundRobin,
            rr_counter: AtomicUsize::new(0),
            health: Mutex::new(HealthState { down_until: vec![] }),
            circuit_break: Duration::from_secs(30),
            retry_attempts: 2,
            fallback_to_write: false,
        })
    }

    fn new_internal(
        write: DatabaseConnection,
        reads: Vec<ReadReplica>,
        routing: &DatabaseRoutingConfig,
    ) -> Self {
        let read_count = reads.len();
        let strategy = match routing.strategy.to_lowercase().as_str() {
            "random" => ReadStrategy::Random,
            "weighted" => ReadStrategy::Weighted,
            _ => ReadStrategy::RoundRobin,
        };
        Self {
            write,
            reads,
            strategy,
            rr_counter: AtomicUsize::new(0),
            health: Mutex::new(HealthState {
                down_until: vec![None; read_count],
            }),
            circuit_break: Duration::from_millis(routing.circuit_break_ms),
            retry_attempts: routing.retry_attempts,
            fallback_to_write: routing.fallback_to_write,
        }
    }

    /// Return the write database connection directly.
    ///
    /// Use this when you need **read-your-writes consistency** — e.g. after
    /// a transaction that modifies data, re-querying the write database
    /// guarantees you see the latest state even if the read replicas are
    /// behind.
    pub fn write_conn(&self) -> &DatabaseConnection {
        &self.write
    }

    /// Return the database backend type of the write database.
    pub fn write_backend(&self) -> DbBackend {
        self.write.get_database_backend()
    }

    /// Start a background health-check task that periodically probes all
    /// currently-down read replicas and removes the circuit breaker when a
    /// replica recovers.
    pub fn start_health_check(self: Arc<Self>, interval: Duration) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                self.probe_reads().await;
            }
        });
    }

    async fn probe_reads(&self) {
        // Collect indices to probe while holding the lock, then probe without the lock
        let to_probe: Vec<usize> = {
            let health = self.health.lock();
            let now = Instant::now();
            let mut result = Vec::new();
            for i in 0..self.reads.len() {
                if let Some(until) = health.down_until[i]
                    && now >= until
                {
                    result.push(i);
                }
            }
            result
        };

        for &i in &to_probe {
            if self.reads[i].conn.ping().await.is_ok() {
                let mut health = self.health.lock();
                // Only clear if still marked as down — a concurrent mark_down
                // may have re-set it after our to_probe collection was taken.
                if health.down_until[i].is_some() {
                    health.down_until[i] = None;
                    tracing::info!("Read replica {} recovered", i);
                }
            } else {
                let mut health = self.health.lock();
                health.down_until[i] = Some(Instant::now() + self.circuit_break);
            }
        }
    }

    // ---- internal routing helpers ----

    /// Select the next healthy read replica index, excluding already-tried ones.
    fn pick_next_read(&self, exclude: &HashSet<usize>) -> Option<usize> {
        if self.reads.is_empty() {
            return None;
        }

        let now = Instant::now();
        // Collect healthy replicas
        let mut healthy: Vec<usize> = Vec::new();
        {
            let mut health = self.health.lock();
            for (i, _) in self.reads.iter().enumerate() {
                if exclude.contains(&i) {
                    continue;
                }
                // Auto-recover expired circuit breakers
                if let Some(until) = health.down_until[i] {
                    if now >= until {
                        health.down_until[i] = None;
                    } else {
                        continue;
                    }
                }
                healthy.push(i);
            }
        }

        if healthy.is_empty() {
            return None;
        }

        let chosen = match self.strategy {
            ReadStrategy::RoundRobin => {
                let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                healthy[idx % healthy.len()]
            }
            ReadStrategy::Random => {
                let mut rng = rand::thread_rng();
                healthy[rng.gen_range(0..healthy.len())]
            }
            ReadStrategy::Weighted => {
                let weights: Vec<u32> = healthy.iter().map(|&i| self.reads[i].weight).collect();
                select_weighted_index(&healthy, &weights, &self.rr_counter)
            }
        };

        Some(chosen)
    }

    /// Execute a read operation with retry logic.
    /// Uses owned `DatabaseConnection` to avoid lifetime issues with async closures.
    async fn execute_read_retry<T, F, Fut>(&self, stmt: Statement, op: F) -> Result<T, DbErr>
    where
        F: Fn(DatabaseConnection, Statement) -> Fut,
        Fut: std::future::Future<Output = Result<T, DbErr>>,
    {
        if self.reads.is_empty() {
            return op(self.write.clone(), stmt).await;
        }

        let mut tried: HashSet<usize> = HashSet::new();
        let mut last_err: Option<DbErr> = None;

        // Phase 1: try each replica once (exclude already-tried ones)
        for _ in 0..self.reads.len() {
            let Some(idx) = self.pick_next_read(&tried) else {
                break;
            };
            tried.insert(idx);

            match op(self.reads[idx].conn.clone(), stmt.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) if is_connection_error(&e) => {
                    self.mark_down(idx);
                    tracing::warn!("Read replica {} failed, marked down: {}", idx, e);
                    last_err = Some(e);
                    continue;
                }
                // Hot-standby recovery conflict (40001): statement cancelled
                // but the connection stays usable — retry another replica
                // WITHOUT marking this one down (the replica is healthy).
                Err(e) if is_recovery_conflict(&e) => {
                    tracing::warn!(
                        "Read replica {} recovery conflict, retrying another replica: {}",
                        idx,
                        e
                    );
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        // Phase 2: extra retry rounds — retry previously-failed replicas directly,
        // bypassing the circuit breaker. The circuit breaker prevents selecting
        // a recently-failed replica in *subsequent* requests; within the same
        // request retry_attempts allows giving each replica a second chance
        // before the circuit breaker fully kicks in.
        if last_err.is_some() && self.retry_attempts > 0 {
            for retry in 0..self.retry_attempts {
                let idx = (tried.len() + retry) % self.reads.len();

                match op(self.reads[idx].conn.clone(), stmt.clone()).await {
                    Ok(v) => return Ok(v),
                    Err(e) if is_connection_error(&e) => {
                        self.mark_down(idx);
                        tracing::warn!(
                            "Read replica {} failed during retry, marked down: {}",
                            idx,
                            e
                        );
                        last_err = Some(e);
                        continue;
                    }
                    Err(e) if is_recovery_conflict(&e) => {
                        tracing::warn!(
                            "Read replica {} recovery conflict during retry, trying next: {}",
                            idx,
                            e
                        );
                        last_err = Some(e);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        if self.fallback_to_write {
            tracing::warn!("All read replicas failed — falling back to writer");
            return op(self.write.clone(), stmt).await;
        }

        Err(last_err.unwrap_or_else(|| DbErr::Custom("all read attempts exhausted".into())))
    }

    fn mark_down(&self, idx: usize) {
        let mut health = self.health.lock();
        if let Some(slot) = health.down_until.get_mut(idx) {
            *slot = Some(Instant::now() + self.circuit_break);
        }
    }
}

/// Select an index from healthy replicas using weighted random selection.
/// Falls back to round-robin when all weights sum to zero.
fn select_weighted_index(healthy: &[usize], weights: &[u32], rr_counter: &AtomicUsize) -> usize {
    let total: u64 = weights.iter().map(|&w| w as u64).sum();
    // total > 0 is guaranteed during normal operation because
    // weight values are min-capped at 1 during connection setup.
    // This guard prevents panics if a future refactor removes the
    // min-cap — if total is 0, fall back to round-robin.
    if total == 0 {
        let idx = rr_counter.fetch_add(1, Ordering::Relaxed);
        healthy[idx % healthy.len()]
    } else {
        let mut rng = rand::thread_rng();
        let mut roll = rng.gen_range(0..total);
        let mut chosen = healthy[0];
        for (&idx, &w) in healthy.iter().zip(weights.iter()) {
            if roll < w as u64 {
                chosen = idx;
                break;
            }
            roll -= w as u64;
        }
        chosen
    }
}

// ---- ConnectionTrait implementation ----

#[async_trait]
impl ConnectionTrait for AutoRouter {
    fn get_database_backend(&self) -> DbBackend {
        self.write.get_database_backend()
    }

    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        self.write.execute_unprepared(sql).await
    }

    fn support_returning(&self) -> bool {
        self.write.support_returning()
    }

    async fn execute(&self, stmt: Statement) -> Result<ExecResult, DbErr> {
        self.write.execute(stmt).await
    }

    async fn query_one(&self, stmt: Statement) -> Result<Option<QueryResult>, DbErr> {
        if is_write_statement(&stmt) || is_locking_select(&stmt) || self.reads.is_empty() {
            tracing::trace!(target = "write", "query_one routed to write");
            return self.write.query_one(stmt).await;
        }
        tracing::trace!(target = "read", "query_one routed to read replicas");
        self.execute_read_retry(stmt, |conn, s| async move { conn.query_one(s).await })
            .await
    }

    async fn query_all(&self, stmt: Statement) -> Result<Vec<QueryResult>, DbErr> {
        if is_write_statement(&stmt) || is_locking_select(&stmt) || self.reads.is_empty() {
            tracing::trace!(target = "write", "query_all routed to write");
            return self.write.query_all(stmt).await;
        }
        tracing::trace!(target = "read", "query_all routed to read replicas");
        self.execute_read_retry(stmt, |conn, s| async move { conn.query_all(s).await })
            .await
    }

    fn is_mock_connection(&self) -> bool {
        false
    }
}

// ---- TransactionTrait implementation ----

#[async_trait]
impl TransactionTrait for AutoRouter {
    async fn begin(&self) -> Result<DatabaseTransaction, DbErr> {
        self.write.begin().await
    }

    async fn begin_with_config(
        &self,
        isolation_level: Option<IsolationLevel>,
        access_mode: Option<AccessMode>,
    ) -> Result<DatabaseTransaction, DbErr> {
        self.write
            .begin_with_config(isolation_level, access_mode)
            .await
    }

    async fn transaction<F, T, E>(&self, txn: F) -> Result<T, TransactionError<E>>
    where
        F: for<'c> FnOnce(
                &'c DatabaseTransaction,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<T, E>> + Send + 'c>,
            > + Send,
        T: Send,
        E: std::fmt::Debug + std::fmt::Display + Send,
    {
        self.write.transaction(txn).await
    }

    async fn transaction_with_config<F, T, E>(
        &self,
        txn: F,
        isolation_level: Option<IsolationLevel>,
        access_mode: Option<AccessMode>,
    ) -> Result<T, TransactionError<E>>
    where
        F: for<'c> FnOnce(
                &'c DatabaseTransaction,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<T, E>> + Send + 'c>,
            > + Send,
        T: Send,
        E: std::fmt::Debug + std::fmt::Display + Send,
    {
        self.write
            .transaction_with_config(txn, isolation_level, access_mode)
            .await
    }
}

// ---- SQL detection ----

/// Detect locking SELECT statements by examining the SQL template rather than
/// the fully-rendered SQL string.
fn is_locking_select(stmt: &Statement) -> bool {
    let sql = stmt.to_string();
    if sql.is_empty() {
        return false;
    }
    let up = sql.to_ascii_uppercase();
    up.contains("FOR UPDATE")
        || up.contains("FOR SHARE")
        || up.contains("FOR NO KEY UPDATE")
        || up.contains("FOR KEY SHARE")
        || up.contains("LOCK IN SHARE MODE")
}

/// Detect write statements that SeaORM routes through `query_one()`/`query_all()`
/// via the `RETURNING` clause on PostgreSQL.
///
/// SeaORM uses `INSERT ... RETURNING *` for `ActiveModel::insert()` and
/// `UPDATE ... RETURNING *` for `ActiveModel::update()`. These generate
/// `query_one()` calls (not `execute()`), so the standard write-routing
/// in `execute()` does not catch them. Without this check they would be
/// incorrectly sent to a read replica.
fn is_write_statement(stmt: &Statement) -> bool {
    let sql = stmt.to_string();
    if sql.is_empty() {
        return false;
    }
    let trimmed = sql.trim_start();
    let up = trimmed.to_ascii_uppercase();

    // Direct write statement detection
    if up.starts_with("INSERT ")
        || up.starts_with("UPDATE ")
        || up.starts_with("DELETE ")
        || up.starts_with("REPLACE ")
    {
        return true;
    }

    // CTE (WITH ...) may wrap a write statement as its main operation.
    // Use a depth-based parser to find the main statement keyword outside
    // of parenthesized CTE subqueries.
    if up.starts_with("WITH ") {
        return cte_main_stmt_is_write(&up);
    }

    false
}

/// Given an uppercase SQL string that starts with "WITH ", determine
/// whether the main statement (after all CTE definitions) is a write
/// operation (INSERT/UPDATE/DELETE/REPLACE).
///
/// Uses parenthesis depth tracking to distinguish CTE subqueries from
/// the outer statement. This is a simplified parser — it does not handle
/// string literals containing parentheses, but such cases are extremely
/// rare in auto-generated SQL and would only cause false positives
/// (routing a SELECT to the write DB), never a correctness failure.
fn cte_main_stmt_is_write(uppercase_sql: &str) -> bool {
    let rest = uppercase_sql.strip_prefix("WITH ").unwrap();
    let rest = rest.strip_prefix("RECURSIVE ").unwrap_or(rest);

    let bytes = rest.as_bytes();
    let mut depth: u32 = 0;
    let mut i = 0;

    while i < bytes.len() {
        let remaining = &bytes[i..];
        match bytes[i] {
            b'(' => depth += 1,
            b')' if depth > 0 => depth -= 1,
            b'I' if depth == 0 && remaining.starts_with(b"INSERT ") => return true,
            b'U' if depth == 0 && remaining.starts_with(b"UPDATE ") => return true,
            b'D' if depth == 0 && remaining.starts_with(b"DELETE ") => return true,
            b'R' if depth == 0 && remaining.starts_with(b"REPLACE ") => return true,
            _ => {}
        }
        i += 1;
    }

    false
}

/// Extract the SQLSTATE code from a `DbErr` when it wraps a PostgreSQL
/// server-reported error (sqlx `Error::Database`).
///
/// 仅用于读路径：sea-orm 将 `query_one`/`query_all` 的 sqlx 错误包装为
/// `DbErr::Query(RuntimeErr::SqlxError(...))`（见 sqlx_common.rs 的
/// `sqlx_error_to_query_err`）；写路径的错误为 `DbErr::Exec` 形态，不经由
/// 此函数（写操作不参与读路由重试）。与 sea-orm 内部 `DbErr::sql_err()`
/// 的 downcast 形态一致：
/// `DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Database(e)))`.
fn pg_sqlstate(e: &DbErr) -> Option<&str> {
    let DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Database(db_err))) = e else {
        return None;
    };
    // `code()` 返回 `Cow<'_, str>`：PG 路径恒为 `Cow::Borrowed`（数据借用自
    // `db_err`，与 `e` 同生命周期），直接解构取出 `&str`；`Cow::Owned` 在真实
    // 驱动中不会出现，保守返回 `None`（避免借用局部变量导致 E0515）。
    match db_err.as_ref().code()? {
        Cow::Borrowed(s) => Some(s),
        Cow::Owned(_) => None,
    }
}

/// Determine whether a `DbErr` likely represents a connection-level
/// failure rather than a query-level error.
///
/// - `DbErr::Conn` is always a connection-level failure (pool timeout,
///   connection refused, etc.).
/// - `DbErr::Query` may wrap sqlx-level connectivity errors (e.g., broken
///   pipe mid-query) or PostgreSQL server-reported FATAL errors (57P01 /
///   57P02 / 57P03). Server-side terminations are classified by their
///   SQLSTATE code; client/OS errors fall back to a conservative keyword
///   set to avoid false positives from legitimate query error messages.
/// - All other variants are never considered connection errors.
fn is_connection_error(e: &DbErr) -> bool {
    // Primary: DbErr::Conn is always a connection-level failure
    if matches!(e, DbErr::Conn(_)) {
        return true;
    }

    // SeaORM 1.1.20+: ConnectionAcquire indicates pool timeout or connection
    // closed — both are connection-level failures. This variant is thrown when
    // acquire_timeout is reached or the pool notices a closed connection,
    // neither of which is a query-level error.
    if matches!(e, DbErr::ConnectionAcquire(_)) {
        return true;
    }

    // Secondary: sqlx may report mid-query connection failures (e.g., broken
    // pipe, transport EOF) as DbErr::Query. PostgreSQL server-side connection
    // termination classes also surface here as FATAL database errors:
    // 57P01 admin_shutdown / 57P02 crash_shutdown terminate the backend
    // mid-query (not a socket-level EOF), 57P03 cannot_connect_now is
    // returned during startup recovery / shutdown. They are classified by
    // SQLSTATE code rather than message text: server messages are localized
    // per LC_MESSAGES and 57P03 has several variants ("starting up" / "in
    // recovery mode" / "shutting down"), while the code is stable. All three
    // indicate the server dropped the connection — the replica must be marked
    // down and the query retried/fallback.
    if matches!(e, DbErr::Query(_)) {
        if matches!(pg_sqlstate(e), Some("57P01" | "57P02" | "57P03")) {
            return true;
        }
        // Keyword fallback for sqlx-level client/OS errors that carry no
        // SQLSTATE code (e.g. sqlx::Error::Io). Conservative set that is
        // extremely unlikely to appear in actual query error messages.
        let s = e.to_string().to_ascii_lowercase();
        let hints = [
            "broken pipe",
            "connection reset",
            "io error",
            "network",
            "eof",
            "transport",
        ];
        return hints.iter().any(|h| s.contains(h));
    }

    false
}

/// Determine whether a `DbErr` represents a hot-standby recovery conflict
/// (SQLSTATE 40001, message "canceling statement due to conflict with
/// recovery").
///
/// During WAL replay on a standby, in-flight queries conflicting with the
/// replayed changes are cancelled after `max_standby_streaming_delay` — the
/// statement is aborted but the connection remains fully usable. The replica
/// itself is healthy, so the query should be retried on another replica
/// WITHOUT marking this one down.
///
/// The match is on the stable SQLSTATE code rather than the message text,
/// which is localized per server LC_MESSAGES. 40001 also covers business
/// serialization failures, which never occur on the read-only standby path
/// this function is used for.
fn is_recovery_conflict(e: &DbErr) -> bool {
    matches!(pg_sqlstate(e), Some("40001"))
}

// ---- connection helpers ----

/// Connect to a database with the given URL and write-pool config.
pub async fn connect_db(url: &str, config: &DatabaseConfig) -> Result<DatabaseConnection, DbErr> {
    let mut opt = ConnectOptions::new(url);
    opt.max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
        .idle_timeout(Duration::from_secs(config.idle_timeout_secs))
        .max_lifetime(Duration::from_secs(1800))
        .test_before_acquire(true);
    Database::connect(opt).await
}

/// Connect to a read replica with the given URL and read-pool config.
async fn connect_db_read(
    url: &str,
    config: &DatabaseReadConfig,
) -> Result<DatabaseConnection, DbErr> {
    let mut opt = ConnectOptions::new(url);
    opt.max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        .idle_timeout(Duration::from_secs(config.idle_timeout_secs))
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
        .max_lifetime(Duration::from_secs(1800))
        .test_before_acquire(true);
    Database::connect(opt).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnAcquireErr, RuntimeErr, Statement};

    // ---- is_locking_select tests ----

    #[test]
    fn test_is_locking_select_for_update() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1 FOR UPDATE".to_string(),
        );
        assert!(is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_for_share() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1 FOR SHARE".to_string(),
        );
        assert!(is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_for_no_key_update() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1 FOR NO KEY UPDATE".to_string(),
        );
        assert!(is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_for_key_share() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1 FOR KEY SHARE".to_string(),
        );
        assert!(is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_lock_in_share_mode() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1 LOCK IN SHARE MODE".to_string(),
        );
        assert!(is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_plain_select() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1".to_string(),
        );
        assert!(!is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_empty() {
        let stmt = Statement::from_string(DbBackend::Postgres, "".to_string());
        assert!(!is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_insert_statement() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "INSERT INTO users (name) VALUES ($1)".to_string(),
        );
        assert!(!is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_update_statement() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "UPDATE users SET name = $1 WHERE id = $2".to_string(),
        );
        assert!(!is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_lowercase_for_update() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "select * from users where id = $1 for update".to_string(),
        );
        assert!(is_locking_select(&stmt));
    }

    #[test]
    fn test_is_locking_select_mixed_case_for_update() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1 For UpDaTe".to_string(),
        );
        assert!(is_locking_select(&stmt));
    }

    // ---- is_write_statement tests ----

    #[test]
    fn test_is_write_statement_insert() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id".to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_update() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "UPDATE users SET name = $1 WHERE id = $2 RETURNING id, name".to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_delete() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "DELETE FROM users WHERE id = $1 RETURNING id".to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_replace() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "REPLACE INTO users (id, name) VALUES ($1, $2)".to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_lowercase_insert() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "insert into users (name) values ($1) returning id".to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_select_is_not_write() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1".to_string(),
        );
        assert!(!is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_with_cte_select_is_not_write() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "WITH recent AS (SELECT * FROM users ORDER BY id DESC LIMIT 10) SELECT * FROM recent"
                .to_string(),
        );
        assert!(!is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_empty() {
        let stmt = Statement::from_string(DbBackend::Postgres, "".to_string());
        assert!(!is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_leading_whitespace() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "  INSERT INTO logs (event) VALUES ($1)".to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    // ---- is_connection_error tests ----

    #[test]
    fn test_is_connection_error_connection_closed() {
        let err = DbErr::Conn(RuntimeErr::Internal("connection closed".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_timeout() {
        let err = DbErr::Conn(RuntimeErr::Internal("pool timed out".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_io_error() {
        let err = DbErr::Conn(RuntimeErr::Internal("IO error: broken pipe".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_broken_pipe() {
        let err = DbErr::Conn(RuntimeErr::Internal("broken pipe".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_network() {
        let err = DbErr::Conn(RuntimeErr::Internal("network is unreachable".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_transport() {
        let err = DbErr::Conn(RuntimeErr::Internal("transport error".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_eof() {
        let err = DbErr::Conn(RuntimeErr::Internal("unexpected eof".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_reset() {
        let err = DbErr::Conn(RuntimeErr::Internal("connection reset by peer".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_acquire_timeout() {
        let err = DbErr::ConnectionAcquire(ConnAcquireErr::Timeout);
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_acquire_connection_closed() {
        let err = DbErr::ConnectionAcquire(ConnAcquireErr::ConnectionClosed);
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_error_not_connection() {
        let err = DbErr::Query(RuntimeErr::Internal(
            "syntax error at or near \"SELECT\"".to_string(),
        ));
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_unique_violation() {
        let err = DbErr::Query(RuntimeErr::Internal("duplicate key value".to_string()));
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_record_not_found() {
        let err = DbErr::RecordNotFound("not found".to_string());
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_case_insensitive() {
        let err = DbErr::Conn(RuntimeErr::Internal("Connection refused".to_string()));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_with_connection_word_is_not_connection_error() {
        // Regression: a DbErr::Query that happens to contain the word "connection"
        // in its error message must NOT be treated as a connection error.
        let err = DbErr::Query(RuntimeErr::Internal(
            "column \"connection_id\" does not exist".to_string(),
        ));
        assert!(!is_connection_error(&err));
    }

    // ── PostgreSQL server-reported errors (SQLSTATE classification) ──────
    //
    // 真实错误路径说明：sea-orm 从不将 sqlx 错误字符串化为
    // RuntimeErr::Internal —— sqlx_common.rs 的 sqlx_error_to_query_err
    // 始终保留 RuntimeErr::SqlxError 形态。下方部分测试（如
    // ignores_internal_string_query）使用 Internal 字符串形态，属于防御性
    // 断言（非真实路径），用于锁定"分类逻辑不依赖消息文本"这一行为。

    /// Mock of sqlx's PostgreSQL server error: mirrors `PgDatabaseError`'s
    /// surface (Display shows only the message) with a fixed SQLSTATE code.
    /// `code` 为 `None` 时模拟 `DatabaseError::code()` 的默认返回（sqlx-core
    /// 默认实现返回 None，如无码错误）。`owned_code` toggles `code()`
    /// between `Cow::Borrowed` (the real PG driver's behaviour) and
    /// `Cow::Owned` (defensive branch coverage).
    #[derive(Debug)]
    struct MockPgError {
        code: Option<&'static str>,
        message: &'static str,
        owned_code: bool,
    }

    impl std::fmt::Display for MockPgError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for MockPgError {}

    impl sqlx::error::DatabaseError for MockPgError {
        fn message(&self) -> &str {
            self.message
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            let code = self.code?;
            if self.owned_code {
                Some(std::borrow::Cow::Owned(code.to_string()))
            } else {
                Some(std::borrow::Cow::Borrowed(code))
            }
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
    }

    /// Build a `DbErr` with the exact shape sea-orm surfaces for a PostgreSQL
    /// server-reported error: `DbErr::Query(RuntimeErr::SqlxError(
    /// sqlx::Error::Database(...)))` (Display: "Query Error: error returned
    /// from database: <message>").
    fn pg_db_err(code: &'static str, message: &'static str) -> DbErr {
        DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Database(Box::new(
            MockPgError {
                code: Some(code),
                message,
                owned_code: false,
            },
        ))))
    }

    #[test]
    fn test_is_connection_error_pg_admin_shutdown_57p01() {
        // PostgreSQL 57P01: backend terminated mid-query by pg_terminate_backend
        // or server shutdown. Reported as a FATAL PgDatabaseError, NOT a
        // socket-level EOF — must still be treated as a connection error so
        // the replica is marked down and the query retried/falls back.
        let err = pg_db_err(
            "57P01",
            "terminating connection due to administrator command",
        );
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_pg_crash_shutdown_57p02() {
        let err = pg_db_err(
            "57P02",
            "terminating connection due to crash of another server process",
        );
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_pg_cannot_connect_now_57p03() {
        let err = pg_db_err("57P03", "the database system is starting up");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_pg_cannot_connect_now_recovery_mode_57p03() {
        // 57P03 has several message variants; SQLSTATE classification must
        // not depend on the exact wording.
        let err = pg_db_err("57P03", "the database system is in recovery mode");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_pg_cannot_connect_now_shutting_down_57p03() {
        let err = pg_db_err("57P03", "the database system is shutting down");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_pg_localized_message_still_matches_by_code() {
        // Regression: server messages are localized per LC_MESSAGES and must
        // not be the basis of classification — the SQLSTATE code is stable.
        let err = pg_db_err(
            "57P01",
            "connexion interrompue par la commande de l'administrateur",
        );
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_recovery_conflict_is_not_connection_error() {
        // Regression: hot-standby recovery conflict (40001) cancels the
        // statement but leaves the connection usable — must NOT be treated
        // as a connection error (no mark_down), but IS retryable.
        let err = pg_db_err("40001", "canceling statement due to conflict with recovery");
        assert!(!is_connection_error(&err));
        assert!(is_recovery_conflict(&err));
    }

    #[test]
    fn test_is_connection_error_cancel_request_57014_is_not_connection_error() {
        // Regression: statement cancellation (57014, e.g. user cancel or
        // statement_timeout) aborts the query but the connection stays
        // usable — neither a connection error nor a recovery conflict.
        let err = pg_db_err("57014", "canceling statement due to user request");
        assert!(!is_connection_error(&err));
        assert!(!is_recovery_conflict(&err));
    }

    #[test]
    fn test_is_recovery_conflict_positive() {
        // Transaction-level variant: "canceling transaction due to conflict
        // with recovery" shares SQLSTATE 40001 and must also match.
        let err = pg_db_err(
            "40001",
            "canceling transaction due to conflict with recovery",
        );
        assert!(is_recovery_conflict(&err));
    }

    #[test]
    fn test_is_recovery_conflict_ignores_conn_variant() {
        // Only DbErr::Query carrying a server-reported Database error can
        // expose a SQLSTATE code; a DbErr::Conn with a coincidental substring
        // must not match.
        let err = DbErr::Conn(RuntimeErr::Internal("conflict with recovery".to_string()));
        assert!(!is_recovery_conflict(&err));
    }

    #[test]
    fn test_is_recovery_conflict_ignores_internal_string_query() {
        // A plain-string DbErr::Query carries no SQLSTATE code, so even a
        // message containing "conflict with recovery" must not match.
        let err = DbErr::Query(RuntimeErr::Internal(
            "canceling statement due to conflict with recovery".to_string(),
        ));
        assert!(!is_recovery_conflict(&err));
    }

    #[test]
    fn test_is_connection_error_query_sqlx_io_broken_pipe() {
        // sqlx-level I/O error mid-query surfaces as DbErr::Query wrapping
        // sqlx::Error::Io — no SQLSTATE code, matched via keyword fallback.
        let err = DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe",
        ))));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_pg_sqlstate_returns_none_for_non_database_errors() {
        assert_eq!(
            pg_sqlstate(&DbErr::Conn(RuntimeErr::Internal("boom".to_string()))),
            None
        );
        assert_eq!(
            pg_sqlstate(&DbErr::Query(RuntimeErr::Internal("boom".to_string()))),
            None
        );
        // sqlx-level I/O error is not a server-reported Database error.
        let io_err = DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe",
        ))));
        assert_eq!(pg_sqlstate(&io_err), None);
    }

    #[test]
    fn test_pg_sqlstate_returns_none_for_owned_code() {
        // Defensive branch: no real driver returns `Cow::Owned` from
        // `code()`, but if one ever did, the code cannot be borrowed from
        // the temporary `Cow` — conservatively report no SQLSTATE code.
        let err = DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Database(Box::new(
            MockPgError {
                code: Some("57P01"),
                message: "terminating connection due to administrator command",
                owned_code: true,
            },
        ))));
        assert_eq!(pg_sqlstate(&err), None);
    }

    #[test]
    fn test_pg_sqlstate_returns_none_when_database_error_has_no_code() {
        // `DatabaseError::code()` 的默认实现返回 None（sqlx-core）；此时没有
        // SQLSTATE 码可分类，pg_sqlstate 必须返回 None 且不 panic。同时验证
        // is_connection_error 走 keyword fallback：消息不含保守关键词（broken
        // pipe 等）时不得误判为连接错误——分类不依赖消息文本这一行为由此锁定。
        let err = DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Database(Box::new(
            MockPgError {
                code: None,
                message: "terminating connection due to administrator command",
                owned_code: false,
            },
        ))));
        assert_eq!(pg_sqlstate(&err), None);
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_with_timeout_word_is_not_connection_error() {
        // Regression: a DbErr::Query containing "timeout" in a query context
        // must NOT be treated as a connection error.
        let err = DbErr::Query(RuntimeErr::Internal("statement timeout".to_string()));
        assert!(!is_connection_error(&err));
    }

    // ---- ReadStrategy tests ----

    #[test]
    fn test_read_strategy_round_robin_default() {
        let strategy = ReadStrategy::RoundRobin;
        assert_eq!(strategy as u8, 0);
    }

    #[test]
    fn test_read_strategy_random() {
        let strategy = ReadStrategy::Random;
        assert_eq!(strategy as u8, 1);
    }

    #[test]
    fn test_read_strategy_weighted() {
        let strategy = ReadStrategy::Weighted;
        assert_eq!(strategy as u8, 2);
    }

    // ---- AutoRouter::single basic test ----
    //
    // NOTE: Full integration tests for AutoRouter require a running PostgreSQL
    // instance. These minimal unit tests verify construction and the no-read-replica
    // path. For read-replica routing tests, see the server integration tests.

    #[test]
    fn test_health_state_default() {
        let health = HealthState {
            down_until: vec![None, None, None],
        };
        assert_eq!(health.down_until.len(), 3);
        assert!(health.down_until.iter().all(|d| d.is_none()));
    }

    #[test]
    fn test_health_state_some_down() {
        let now = Instant::now();
        let health = HealthState {
            down_until: vec![Some(now), None],
        };
        assert!(health.down_until[0].is_some());
        assert!(health.down_until[1].is_none());
    }

    // ---- is_write_statement edge cases ----

    #[test]
    fn test_is_write_statement_truncate_not_write() {
        // TRUNCATE is a DDL statement, not a DML write-through-query.
        // It should NOT be caught by is_write_statement (it goes through
        // execute(), not query_one/query_all).
        let stmt = Statement::from_string(DbBackend::Postgres, "TRUNCATE TABLE users".to_string());
        assert!(!is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_with_cte_insert() {
        // A WITH ... INSERT is still an INSERT statement.
        // With the CTE depth-based parser, this is now correctly detected.
        // The DELETE inside the CTE subquery is at depth > 0 and ignored;
        // the INSERT at depth 0 is detected as the main write operation.
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "WITH deleted AS (DELETE FROM logs WHERE created_at < $1 RETURNING *) INSERT INTO audit SELECT * FROM deleted".to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_upsert_detected_as_insert() {
        // INSERT ... ON CONFLICT (UPSERT) starts with "INSERT ",
        // so is_write_statement correctly detects it as a write operation.
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "INSERT INTO users (id, name) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET name = $3"
                .to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    // ---- CTE write statement tests (enhanced CTE detection) ----

    #[test]
    fn test_is_write_statement_with_cte_update() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "WITH to_update AS (SELECT * FROM users WHERE status = 'inactive') UPDATE users SET status = 'archived' FROM to_update WHERE users.id = to_update.id"
                .to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_with_cte_delete() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "WITH expired AS (SELECT id FROM sessions WHERE expires_at < NOW()) DELETE FROM sessions WHERE id IN (SELECT id FROM expired)"
                .to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_with_cte_recursive_insert() {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "WITH RECURSIVE org_tree AS (SELECT id, parent_id FROM orgs WHERE id = $1 UNION ALL SELECT o.id, o.parent_id FROM orgs o JOIN org_tree t ON o.parent_id = t.id) INSERT INTO audit_log (org_id, action) SELECT id, 'deleted' FROM org_tree"
                .to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_with_cte_multiple_insert() {
        // Multiple CTEs followed by an INSERT
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "WITH deleted_orders AS (DELETE FROM orders WHERE status = 'cancelled' RETURNING *), archived_products AS (UPDATE products SET archived = true WHERE stock = 0 RETURNING *) INSERT INTO audit (event, table_name) SELECT 'deleted', 'orders' FROM deleted_orders UNION ALL SELECT 'archived', 'products' FROM archived_products"
                .to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_with_cte_nested_parens() {
        // CTE subquery with deeply nested parentheses
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "WITH filtered AS (SELECT * FROM (SELECT * FROM (SELECT * FROM users WHERE id IN (1, 2, 3)) t1) t2) DELETE FROM users USING filtered WHERE users.id = filtered.id"
                .to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_with_cte_select_count_not_write() {
        // CTE with aggregation SELECT — NOT a write
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "WITH user_counts AS (SELECT role, count(*) as cnt FROM users GROUP BY role) SELECT role, cnt FROM user_counts ORDER BY cnt DESC"
                .to_string(),
        );
        assert!(!is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_cte_unchanged_for_direct_inserts() {
        // Regular INSERT must still be detected (regression check)
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id".to_string(),
        );
        assert!(is_write_statement(&stmt));
    }

    #[test]
    fn test_is_write_statement_cte_unchanged_for_direct_select() {
        // Regular SELECT must still NOT be detected (regression check)
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM users WHERE id = $1".to_string(),
        );
        assert!(!is_write_statement(&stmt));
    }

    // ---- select_weighted_index tests ----

    #[test]
    fn test_select_weighted_index_single_element() {
        let counter = AtomicUsize::new(0);
        let result = select_weighted_index(&[0], &[5], &counter);
        assert_eq!(result, 0);
    }

    #[test]
    fn test_select_weighted_index_zero_weights_fallback_round_robin() {
        let counter = AtomicUsize::new(0);
        // All weights are 0 — fall back to round-robin.
        // With healthy = [0, 1, 2] and counter starting at 0:
        //   call 1: fetch_add(0) = 0 -> 0 %% 3 = 0
        //   call 2: fetch_add(1) = 1 -> 1 %% 3 = 1
        //   call 3: fetch_add(2) = 2 -> 2 %% 3 = 2
        //   call 4: fetch_add(3) = 3 -> 3 %% 3 = 0
        let healthy = [0usize, 1, 2];
        let weights = [0u32, 0, 0];

        assert_eq!(select_weighted_index(&healthy, &weights, &counter), 0);
        assert_eq!(select_weighted_index(&healthy, &weights, &counter), 1);
        assert_eq!(select_weighted_index(&healthy, &weights, &counter), 2);
        assert_eq!(select_weighted_index(&healthy, &weights, &counter), 0);
    }

    #[test]
    fn test_select_weighted_index_mixed_weights_fallback_guard() {
        // Some zero weights, but total > 0 — should not fall back to round-robin.
        // The RNG makes this non-deterministic, but we can verify the returned
        // index is always from the healthy set (not a bounds error).
        let counter = AtomicUsize::new(100);
        let healthy = [0usize, 1, 2];
        let weights = [3u32, 0, 1];

        // Run multiple times and verify the result is always valid
        for _ in 0..200 {
            let idx = select_weighted_index(&healthy, &weights, &counter);
            assert!(idx < 3, "Index {} out of bounds", idx);
        }
    }

    #[test]
    fn test_select_weighted_index_skips_index_if_healthy_filtered() {
        // healthy = [1, 2] means only replicas 1 and 2 are available.
        // weights = [5, 10] maps to healthy indices [1, 2] by position.
        let counter = AtomicUsize::new(0);
        let healthy = [1usize, 2];
        let weights = [5u32, 10];

        for _ in 0..100 {
            let idx = select_weighted_index(&healthy, &weights, &counter);
            assert!(idx == 1 || idx == 2, "Expected index 1 or 2, got {}", idx);
        }
    }

    // ---- is_connection_error additional edge cases ----

    #[test]
    fn test_is_connection_error_query_broken_pipe() {
        // SQLx may report connection failures as DbErr::Query
        let err = DbErr::Query(RuntimeErr::Internal(
            "broken pipe: the server closed the connection".to_string(),
        ));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_connection_reset() {
        let err = DbErr::Query(RuntimeErr::Internal(
            "connection reset by peer while reading data".to_string(),
        ));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_io_error() {
        let err = DbErr::Query(RuntimeErr::Internal(
            "IO error: unexpected EOF reading from socket".to_string(),
        ));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_network_unreachable() {
        let err = DbErr::Query(RuntimeErr::Internal(
            "network is unreachable while connecting to host".to_string(),
        ));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_transport_tls() {
        let err = DbErr::Query(RuntimeErr::Internal(
            "transport layer error: TLS handshake failed".to_string(),
        ));
        assert!(is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_custom_not_connection() {
        // DbErr::Custom should never be treated as a connection error
        let err = DbErr::Custom("connection pool exhausted".to_string());
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_syntax_error_not_connection() {
        // Syntax errors must not be mistaken for connection issues
        let err = DbErr::Query(RuntimeErr::Internal(
            "syntax error at or near \"INSERT\"".to_string(),
        ));
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_relation_not_found() {
        let err = DbErr::Query(RuntimeErr::Internal(
            "relation \"users\" does not exist".to_string(),
        ));
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_deadlock_not_connection() {
        let err = DbErr::Query(RuntimeErr::Internal(
            "deadlock detected while waiting for resource".to_string(),
        ));
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_query_serialization_failure() {
        let err = DbErr::Query(RuntimeErr::Internal(
            "could not serialize access due to concurrent update".to_string(),
        ));
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_conn_acquire_unknown() {
        let err = DbErr::ConnectionAcquire(ConnAcquireErr::Timeout);
        assert!(is_connection_error(&err));
    }

    // ---- HealthState mark_down / probe behavior tests ----

    #[test]
    fn test_health_state_mark_down_updates_timer() {
        let mut health = HealthState {
            down_until: vec![None, None],
        };
        let now = Instant::now();
        health.down_until[0] = Some(now);
        assert!(health.down_until[0].is_some());
        assert!(health.down_until[0].unwrap() >= now);
        assert!(health.down_until[1].is_none());
    }

    #[test]
    fn test_health_state_expired_is_none_or_future() {
        let past = Instant::now() - Duration::from_secs(60);
        let future = Instant::now() + Duration::from_secs(60);
        let health = HealthState {
            down_until: vec![Some(past), Some(future), None],
        };
        assert!(health.down_until[0].is_some());
        assert!(health.down_until[0].unwrap() < Instant::now());
        assert!(health.down_until[1].unwrap() > Instant::now());
        assert!(health.down_until[2].is_none());
    }

    // ---- is_connection_error negative tests for all non-connection DbErr variants ----
    //
    // These tests verify that is_connection_error returns false for every DbErr
    // variant that does NOT represent a connection-level failure.

    #[test]
    fn test_is_connection_error_exec_not_connection() {
        // Execution errors (e.g., constraint violations at execute time) are
        // NOT connection-level failures and must not trigger a circuit break.
        let err = DbErr::Exec(RuntimeErr::Internal(
            "duplicate key value violates unique constraint".to_string(),
        ));
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_type_not_connection() {
        let err = DbErr::Type("Expected i32, got String".to_string());
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_json_not_connection() {
        let err = DbErr::Json("missing field `email`".to_string());
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_migration_not_connection() {
        let err = DbErr::Migration("already applied".to_string());
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_convert_from_u64_not_connection() {
        let err = DbErr::ConvertFromU64("bool");
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_try_into_err_not_connection() {
        let err = DbErr::TryIntoErr {
            from: "i32",
            into: "String",
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "conversion failed",
            )),
        };
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_unpack_insert_id_not_connection() {
        let err = DbErr::UnpackInsertId;
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_update_get_primary_key_not_connection() {
        let err = DbErr::UpdateGetPrimaryKey;
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_record_not_inserted_not_connection() {
        let err = DbErr::RecordNotInserted;
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_record_not_updated_not_connection() {
        let err = DbErr::RecordNotUpdated;
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn test_is_connection_error_attr_not_set_not_connection() {
        let err = DbErr::AttrNotSet("name".to_string());
        assert!(!is_connection_error(&err));
    }

    // ---- execute_read_retry 集成测试（假 op + Disconnected 连接）──────────
    //
    // 不启用 sea-orm mock feature（启用后 DatabaseConnection 不再实现 Clone，
    // 会破坏生产代码的 conn.clone()）。改用公开的 DatabaseConnection::Disconnected
    // 变体 + 计数器闭包：execute_read_retry 内部对每个副本调用传入的 op，
    // 由计数器区分调用次序（第 N 个副本返回预置结果/错误），从而验证
    // mark_down / 重试 / fallback 编排与 SQLSTATE 分类的联动。

    /// 构造一个写库 + 两个读副本（RoundRobin，连接均为 Disconnected）的
    /// AutoRouter。`fallback_to_write` 控制全部失败后的写库回退，
    /// `retry_attempts` 控制 Phase 2 重试轮数。
    fn mock_router(fallback_to_write: bool, retry_attempts: usize) -> AutoRouter {
        AutoRouter {
            write: DatabaseConnection::Disconnected,
            reads: vec![
                ReadReplica {
                    conn: DatabaseConnection::Disconnected,
                    original_index: 0,
                    weight: 1,
                },
                ReadReplica {
                    conn: DatabaseConnection::Disconnected,
                    original_index: 1,
                    weight: 1,
                },
            ],
            strategy: ReadStrategy::RoundRobin,
            rr_counter: AtomicUsize::new(0),
            health: Mutex::new(HealthState {
                down_until: vec![None, None],
            }),
            circuit_break: Duration::from_secs(30),
            retry_attempts,
            fallback_to_write,
        }
    }

    fn select_stmt() -> Statement {
        Statement::from_string(DbBackend::Postgres, "SELECT 1".to_string())
    }

    /// 测试用 op 类型：模拟一次读查询（忽略传入的连接与语句）。
    type FailingOp = Box<
        dyn Fn(
            DatabaseConnection,
            Statement,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DbErr>> + Send>>,
    >;

    /// 构造一个按调用次数返回错误的 op：前 `err_count` 次调用返回 `err`
    /// （每次通过 `fn() -> DbErr` 构造新错误），其余返回 `Ok(())`。
    /// `err_count == 0` 时恒成功。`Fn` 即可多次调用（execute_read_retry
    /// 内部从不 clone op，故无需 Copy）。
    /// 返回 `(op, calls)`：`calls` 记录实际调用次数，用于断言"恰好尝试了
    /// 预期的副本数"（含 Phase 2 与 fallback）。
    ///
    /// 注意：`err_count` 按调用次序消费，调用次序 = RoundRobin 策略 +
    /// `retry_attempts` 配置下的副本次序（由 mock_router 固定）。若修改
    /// 策略或重试次数，需同步调整 `err_count` 的语义。
    fn failing_op(err_count: usize, err: fn() -> DbErr) -> (FailingOp, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);
        (
            Box::new(move |_conn, _stmt| {
                let n = calls_ref.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { if n < err_count { Err(err()) } else { Ok(()) } })
            }),
            calls,
        )
    }

    #[tokio::test]
    async fn test_execute_read_retry_57p01_marks_down_and_retries_healthy_replica() {
        // 副本 0 返回 57P01（连接被服务端终止）→ 应被 mark_down；
        // 副本 1 健康 → 查询成功，且健康副本不被误标。
        let router = mock_router(false, 0);
        let (op, calls) = failing_op(1, || {
            pg_db_err(
                "57P01",
                "terminating connection due to administrator command",
            )
        });

        let result = router.execute_read_retry(select_stmt(), op).await;
        assert!(result.is_ok());

        let health = router.health.lock();
        assert!(health.down_until[0].is_some(), "57P01 副本应被标记 down");
        assert!(health.down_until[1].is_none(), "健康副本不应被标记 down");
        drop(health);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "每个副本恰好尝试一次");
    }

    #[tokio::test]
    async fn test_execute_read_retry_recovery_conflict_does_not_mark_down() {
        // 40001（热备恢复冲突）取消语句但连接可用 → 不 mark_down，
        // 重试另一副本成功。
        let router = mock_router(false, 0);
        let (op, calls) = failing_op(1, || {
            pg_db_err("40001", "canceling statement due to conflict with recovery")
        });

        let result = router.execute_read_retry(select_stmt(), op).await;
        assert!(result.is_ok());

        let health = router.health.lock();
        assert!(
            health.down_until[0].is_none(),
            "40001 副本保持健康（不 mark_down）"
        );
        assert!(health.down_until[1].is_none());
        drop(health);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "每个副本恰好尝试一次");
    }

    #[tokio::test]
    async fn test_execute_read_retry_all_replicas_down_returns_error() {
        // 两个副本都返回连接终止类错误 → 全部 mark_down；
        // 无 fallback 时返回错误。
        let router = mock_router(false, 0);
        let (op, calls) = failing_op(2, || {
            pg_db_err(
                "57P01",
                "terminating connection due to administrator command",
            )
        });

        let result = router.execute_read_retry(select_stmt(), op).await;
        assert!(result.is_err());

        let health = router.health.lock();
        assert!(health.down_until[0].is_some());
        assert!(health.down_until[1].is_some());
        drop(health);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "每个副本恰好尝试一次");
    }

    #[tokio::test]
    async fn test_execute_read_retry_all_replicas_down_falls_back_to_write() {
        // 两个副本均失败且 fallback_to_write=true → 回退到写库成功
        // （op 第 3 次调用——即写库——返回 Ok）。
        let router = mock_router(true, 0);
        let (op, calls) = failing_op(2, || pg_db_err("57P01", "connection terminated"));

        let result = router.execute_read_retry(select_stmt(), op).await;
        assert!(result.is_ok());

        let health = router.health.lock();
        assert!(health.down_until[0].is_some());
        assert!(health.down_until[1].is_some());
        drop(health);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "两个副本各一次 + fallback 一次"
        );
    }

    #[tokio::test]
    async fn test_execute_read_retry_phase2_gives_failed_replica_second_chance() {
        // retry_attempts=1：首轮两个副本均 57P01（均 mark_down）；Phase 2
        // 绕过熔断直接重试副本 0（n=2）成功——验证"第二次机会"语义：
        // 查询成功且两个副本都曾被标记 down。
        let router = mock_router(false, 1);
        let (op, calls) = failing_op(2, || pg_db_err("57P01", "connection terminated"));

        let result = router.execute_read_retry(select_stmt(), op).await;
        assert!(result.is_ok());

        let health = router.health.lock();
        assert!(
            health.down_until[0].is_some(),
            "Phase 1 中副本 0 被 mark_down"
        );
        assert!(
            health.down_until[1].is_some(),
            "Phase 1 中副本 1 被 mark_down"
        );
        drop(health);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "Phase 1 两次 + Phase 2 一次"
        );
    }

    #[tokio::test]
    async fn test_execute_read_retry_phase2_recovery_conflict_does_not_mark_down() {
        // retry_attempts=1：首轮两个副本均 40001（不 mark_down）；Phase 2
        // 重试副本 0 仍 40001 → 仍不 mark_down；无 fallback → 返回错误。
        let router = mock_router(false, 1);
        let (op, calls) = failing_op(3, || {
            pg_db_err("40001", "canceling statement due to conflict with recovery")
        });

        let result = router.execute_read_retry(select_stmt(), op).await;
        assert!(result.is_err());

        let health = router.health.lock();
        assert!(
            health.down_until[0].is_none(),
            "40001 在 Phase 2 也不 mark_down"
        );
        assert!(health.down_until[1].is_none());
        drop(health);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "Phase 1 两次 + Phase 2 一次"
        );
    }
}
