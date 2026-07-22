//! PostgreSQL storage for the Fusillade scheduling daemon.

use std::future::Future;
use std::time::Duration;

use fusillade_core::FusilladeError;

mod db;
pub mod postgres;
#[path = "response_step.rs"]
pub mod postgres_response_step;
pub mod transform;
mod utils;

pub use fusillade_core::manager::{
    ArchiveOutcome, DaemonStorage, ModelFilter, ModelFilterState, Storage,
};
pub use fusillade_core::request::AnyRequest;
pub use fusillade_core::response_step;
pub use postgres::{BatchInsertStrategy, PoolProvider, PostgresRequestManager, TestDbPools};
pub use postgres_response_step::PostgresResponseStepManager;
pub use transform::ResponseTransformer;

pub mod batch {
    pub use fusillade_core::batch::*;
}

pub mod daemon {
    pub use crate::PostgresStorageConfig as DaemonConfig;
    pub use fusillade_core::daemon_record::*;
}

pub mod manager {
    pub use fusillade_core::manager::*;
}

pub mod error {
    pub use fusillade_core::error::*;
}

pub mod request {
    pub use fusillade_core::request::*;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PostgresStorageConfig {
    #[serde(default = "default_pending_request_counts_timeout_ms")]
    pub pending_request_counts_timeout_ms: u64,
    /// Maximum number of request state transitions that may write to Postgres
    /// concurrently. Set to `0` to disable the limit.
    #[serde(default = "default_max_concurrent_state_writes")]
    pub max_concurrent_state_writes: usize,
    #[serde(default = "default_batch_metadata_fields")]
    pub batch_metadata_fields: Vec<String>,
    pub claim_timeout_ms: u64,
    pub processing_timeout_ms: u64,
    pub stale_daemon_threshold_ms: u64,
    pub unclaim_batch_size: usize,
    #[serde(default = "default_service_tier_completion_windows_ms")]
    pub service_tier_completion_windows_ms: std::collections::HashMap<String, u64>,
    #[serde(default = "default_completion_window_ms")]
    pub default_completion_window_ms: u64,
    #[serde(default = "default_claim_ramp_exponent")]
    pub claim_ramp_exponent: f64,
    #[serde(default)]
    pub urgency_weight: f64,
    #[serde(default)]
    pub batch_claim_require_live: bool,
    /// Database-wide per-model in-flight ceiling below which explicitly
    /// requested background backlog is claimable and exposed by pending-count
    /// queries. Zero hides background demand and disables processing at the
    /// daemon layer.
    #[serde(default)]
    pub background_concurrency_limit: usize,
    #[serde(default = "default_leaks_per_window")]
    pub leaks_per_window: f64,
    #[serde(default = "default_model_filters_keep_per_model")]
    pub model_filters_keep_per_model: i64,
    #[serde(default = "default_model_filters_retention_ms")]
    pub model_filters_retention_ms: u64,
}

fn default_batch_metadata_fields() -> Vec<String> {
    vec![
        "id".to_string(),
        "endpoint".to_string(),
        "created_at".to_string(),
        "completion_window".to_string(),
    ]
}

fn default_service_tier_completion_windows_ms() -> std::collections::HashMap<String, u64> {
    std::collections::HashMap::from([("flex".to_string(), 3_600_000)])
}

fn default_completion_window_ms() -> u64 {
    86_400_000
}

fn default_pending_request_counts_timeout_ms() -> u64 {
    60_000
}

fn default_max_concurrent_state_writes() -> usize {
    64
}

fn default_claim_ramp_exponent() -> f64 {
    0.56
}

fn default_leaks_per_window() -> f64 {
    60.0
}

fn default_model_filters_keep_per_model() -> i64 {
    50
}

fn default_model_filters_retention_ms() -> u64 {
    604_800_000
}

impl Default for PostgresStorageConfig {
    fn default() -> Self {
        Self {
            pending_request_counts_timeout_ms: default_pending_request_counts_timeout_ms(),
            max_concurrent_state_writes: default_max_concurrent_state_writes(),
            batch_metadata_fields: default_batch_metadata_fields(),
            claim_timeout_ms: 60_000,
            processing_timeout_ms: 600_000,
            stale_daemon_threshold_ms: 30_000,
            unclaim_batch_size: 100,
            service_tier_completion_windows_ms: default_service_tier_completion_windows_ms(),
            default_completion_window_ms: default_completion_window_ms(),
            claim_ramp_exponent: default_claim_ramp_exponent(),
            urgency_weight: 0.0,
            batch_claim_require_live: false,
            background_concurrency_limit: 0,
            leaks_per_window: default_leaks_per_window(),
            model_filters_keep_per_model: default_model_filters_keep_per_model(),
            model_filters_retention_ms: default_model_filters_retention_ms(),
        }
    }
}

/// Retry cadence for transient database failures.
///
/// Each entry is the delay before the next retry. An empty cadence disables
/// retries and preserves the first error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbRetryConfig {
    retry_delays: Vec<Duration>,
}

impl DbRetryConfig {
    pub fn new(retry_delays: Vec<Duration>) -> Self {
        Self { retry_delays }
    }

    pub fn fixed(retries: usize, delay: Duration) -> Self {
        Self {
            retry_delays: vec![delay; retries],
        }
    }

    pub fn disabled() -> Self {
        Self::new(Vec::new())
    }

    pub fn retry_delays(&self) -> &[Duration] {
        &self.retry_delays
    }
}

impl Default for DbRetryConfig {
    fn default() -> Self {
        Self::fixed(3, Duration::from_millis(50))
    }
}

pub async fn retry_transient_db_errors<T, Op, Fut>(
    config: &DbRetryConfig,
    mut operation: Op,
) -> fusillade_core::Result<T>
where
    Op: FnMut() -> Fut,
    Fut: Future<Output = fusillade_core::Result<T>>,
{
    for delay in config.retry_delays() {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if is_retryable_db_error(&error) => {
                if !delay.is_zero() {
                    tokio::time::sleep(*delay).await;
                }
            }
            Err(error) => return Err(error),
        }
    }

    operation().await
}

pub fn is_retryable_db_error(error: &FusilladeError) -> bool {
    is_retryable_db_error_message(&error.to_string())
}

pub fn is_retryable_db_error_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("pool timed out while waiting for an open connection")
        || message.contains("pooltimedout")
        || message.contains("connection pool timed out")
}

struct ConcurrentIndexMigration {
    version: i64,
    name: &'static str,
    create_sql: &'static str,
}

const BACKGROUND_CONCURRENT_INDEX_MIGRATIONS: &[ConcurrentIndexMigration] = &[
    ConcurrentIndexMigration {
        version: 20260722000100,
        name: "idx_requests_pending_background_batchless",
        create_sql: r#"CREATE INDEX CONCURRENTLY idx_requests_pending_background_batchless
            ON requests (model, created_at, id)
            WHERE state = 'pending'
              AND batch_id IS NULL
              AND template_id IS NOT NULL
              AND service_tier = 'background'"#,
    },
    ConcurrentIndexMigration {
        version: 20260722000200,
        name: "idx_requests_pending_background_batched",
        create_sql: r#"CREATE INDEX CONCURRENTLY idx_requests_pending_background_batched
            ON requests (model, batch_id, created_at, id)
            WHERE state = 'pending'
              AND batch_id IS NOT NULL
              AND template_id IS NOT NULL
              AND service_tier = 'background'"#,
    },
    ConcurrentIndexMigration {
        version: 20260722000300,
        name: "idx_requests_pending_batchless_sla",
        create_sql: r#"CREATE INDEX CONCURRENTLY idx_requests_pending_batchless_sla
            ON requests (model, created_at, id)
            WHERE state = 'pending'
              AND batch_id IS NULL
              AND template_id IS NOT NULL
              AND service_tier IS DISTINCT FROM 'background'"#,
    },
    ConcurrentIndexMigration {
        version: 20260722000400,
        name: "idx_requests_active_sla_counts",
        create_sql: r#"CREATE INDEX CONCURRENTLY idx_requests_active_sla_counts
            ON requests (batch_id, model)
            WHERE state IN ('pending', 'claimed', 'processing')
              AND template_id IS NOT NULL
              AND (
                  service_tier IS NULL
                  OR service_tier NOT IN ('priority', 'background')
              )"#,
    },
];

async fn repair_interrupted_background_index_migrations(
    connection: &mut sqlx::PgConnection,
) -> Result<(), sqlx::migrate::MigrateError> {
    let migration_table_exists =
        sqlx::query_scalar::<_, bool>("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await?;
    if !migration_table_exists {
        return Ok(());
    }

    for index in BACKGROUND_CONCURRENT_INDEX_MIGRATIONS {
        let validity = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT metadata.indisvalid
            FROM pg_index metadata
            WHERE metadata.indexrelid = to_regclass($1)
            "#,
        )
        .bind(index.name)
        .fetch_optional(&mut *connection)
        .await?;
        let migration_applied = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM _sqlx_migrations
                WHERE version = $1 AND success
            )
            "#,
        )
        .bind(index.version)
        .fetch_one(&mut *connection)
        .await?;

        match (validity, migration_applied) {
            (Some(false), false) => {
                // A cancelled CREATE INDEX CONCURRENTLY leaves an invalid
                // relation behind. Remove it so SQLx can retry the unrecorded
                // migration instead of accepting IF NOT EXISTS as success.
                sqlx::query(&format!("DROP INDEX CONCURRENTLY IF EXISTS {}", index.name))
                    .execute(&mut *connection)
                    .await?;
            }
            (Some(false), true) => {
                // If SQLx already recorded the migration, rebuild explicitly.
                // Keeping DROP and CREATE as separate protocol messages lets
                // both operations remain concurrent and retryable.
                sqlx::query(&format!("DROP INDEX CONCURRENTLY IF EXISTS {}", index.name))
                    .execute(&mut *connection)
                    .await?;
                sqlx::query(index.create_sql)
                    .execute(&mut *connection)
                    .await?;
            }
            (None, true) => {
                // Restore an applied index that was removed during manual
                // recovery before accepting new work.
                sqlx::query(index.create_sql)
                    .execute(&mut *connection)
                    .await?;
            }
            (Some(true), _) | (None, false) => {}
        }
    }

    Ok(())
}

/// Fusillade Arsenal database migrator.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Repair interrupted concurrent index builds and run all Arsenal migrations.
///
/// Production callers should prefer this over invoking [`MIGRATOR`] directly.
/// PostgreSQL can leave an invalid relation when `CREATE INDEX CONCURRENTLY` is
/// interrupted; this entry point serializes migration runners and repairs that
/// state before SQLx evaluates `IF NOT EXISTS`.
pub async fn run_migrations(pool: &sqlx::PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    const LOCK: &str =
        "SELECT pg_advisory_lock(hashtextextended('fusillade.concurrent-index-migrations', 0))";
    const UNLOCK: &str =
        "SELECT pg_advisory_unlock(hashtextextended('fusillade.concurrent-index-migrations', 0))";

    // Detaching ensures cancellation closes the connection and releases the
    // session-level advisory lock instead of returning a locked connection to
    // the pool.
    let mut connection = pool.acquire().await?.detach();
    sqlx::query(LOCK).execute(&mut connection).await?;

    let migration_result = async {
        repair_interrupted_background_index_migrations(&mut connection).await?;
        MIGRATOR.run(&mut connection).await
    }
    .await;
    let unlock_result = sqlx::query(UNLOCK).execute(&mut connection).await;

    match migration_result {
        Err(error) => Err(error),
        Ok(()) => {
            unlock_result?;
            Ok(())
        }
    }
}

/// Get the Fusillade Arsenal database migrator.
///
/// Returns a migrator that can be run against a PostgreSQL pool.
pub fn migrator() -> &'static sqlx::migrate::Migrator {
    &MIGRATOR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn migration_runner_restores_an_applied_concurrent_index(pool: sqlx::PgPool) {
        sqlx::query("DROP INDEX CONCURRENTLY idx_requests_pending_background_batchless")
            .execute(&pool)
            .await
            .unwrap();

        run_migrations(&pool).await.unwrap();

        let valid = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT metadata.indisvalid
            FROM pg_index metadata
            WHERE metadata.indexrelid = to_regclass(
                'idx_requests_pending_background_batchless'
            )
            "#,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(valid);
    }

    #[test]
    fn state_write_concurrency_defaults_when_missing() {
        let mut serialized = serde_json::to_value(PostgresStorageConfig::default()).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("max_concurrent_state_writes");

        let decoded: PostgresStorageConfig = serde_json::from_value(serialized).unwrap();
        let reencoded = serde_json::to_value(decoded).unwrap();

        assert_eq!(reencoded["max_concurrent_state_writes"], 64);
    }

    #[test]
    fn state_write_concurrency_explicit_value_round_trips() {
        let mut serialized = serde_json::to_value(PostgresStorageConfig::default()).unwrap();
        serialized["max_concurrent_state_writes"] = serde_json::json!(17);

        let decoded: PostgresStorageConfig = serde_json::from_value(serialized).unwrap();
        let reencoded = serde_json::to_value(decoded).unwrap();

        assert_eq!(reencoded["max_concurrent_state_writes"], 17);
    }
}
