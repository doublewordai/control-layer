//! PostgreSQL storage for the Fusillade scheduling daemon.

use std::future::Future;
use std::time::Duration;

use fusillade_core::FusilladeError;

mod db;
pub mod postgres;
pub mod transform;
mod utils;

pub use fusillade_core::manager::{
    ArchiveOutcome, BackgroundClaimKind, DaemonStorage, ModelFilter, ModelFilterState,
    RetainedResponseWriteError, RetentionPolicy, Storage,
};
pub use fusillade_core::request::AnyRequest;
pub use postgres::{
    BatchInsertStrategy, DynPools, PoolHandle, PoolProvider, PostgresRequestManager, TestDbPools,
};
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
    /// Database-wide per-model foreground in-flight threshold below which
    /// explicitly requested background backlog is claimable and exposed by
    /// pending-count queries. Active background work does not consume this
    /// threshold. Zero hides background demand and disables processing at the
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

/// Fusillade Arsenal database migrator.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Get the Fusillade Arsenal database migrator.
///
/// Returns a migrator that can be run against a PostgreSQL pool.
pub fn migrator() -> &'static sqlx::migrate::Migrator {
    &MIGRATOR
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[sqlx::test(migrations = false)]
    async fn background_schema_migration_is_atomic(pool: sqlx::PgPool) {
        let baseline = sqlx::migrate::Migrator {
            migrations: Cow::Owned(
                MIGRATOR
                    .iter()
                    .filter(|migration| migration.version < 20260722000000)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: false,
            locking: true,
            no_tx: false,
        };
        baseline.run(&pool).await.unwrap();

        // Force a late constraint-name collision after the migration has
        // changed request constraints and added the batch service-tier column.
        sqlx::query(
            "ALTER TABLE batches ADD CONSTRAINT batches_background_deadline_check CHECK (TRUE)",
        )
        .execute(&pool)
        .await
        .unwrap();

        MIGRATOR
            .run(&pool)
            .await
            .expect_err("the conflicting constraint must reject the background migration");

        let service_tier_column_exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM information_schema.columns
                WHERE table_schema = current_schema()
                  AND table_name = 'batches'
                  AND column_name = 'service_tier'
            )
            "#,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !service_tier_column_exists,
            "a late DDL failure must roll back the earlier background schema changes"
        );

        let migration_applied = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 20260722000000)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !migration_applied,
            "an atomic failure must not record the migration as applied"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn background_schema_migration_accepts_prebuilt_indexes(pool: sqlx::PgPool) {
        let baseline = sqlx::migrate::Migrator {
            migrations: Cow::Owned(
                MIGRATOR
                    .iter()
                    .filter(|migration| migration.version < 20260722000000)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: false,
            locking: true,
            no_tx: false,
        };
        baseline.run(&pool).await.unwrap();

        // Production builds these indexes concurrently before deploying the
        // release. The migration must accept the pre-created column and index
        // names so its remaining metadata-only changes stay cheap.
        sqlx::query("ALTER TABLE batches ADD COLUMN service_tier TEXT")
            .execute(&pool)
            .await
            .unwrap();

        for statement in [
            r#"
            CREATE INDEX idx_batches_background_active
            ON batches (created_at, id)
            WHERE service_tier = 'background'
              AND deleted_at IS NULL
            "#,
            r#"
            CREATE INDEX idx_requests_pending_background_batchless
            ON requests (model, created_at, id)
            WHERE state = 'pending'
              AND batch_id IS NULL
              AND template_id IS NOT NULL
              AND service_tier = 'background'
            "#,
            r#"
            CREATE INDEX idx_requests_pending_background_batched
            ON requests (model, batch_id, created_at, id)
            WHERE state = 'pending'
              AND batch_id IS NOT NULL
              AND template_id IS NOT NULL
              AND service_tier = 'background'
            "#,
            r#"
            CREATE INDEX idx_requests_pending_batchless_sla
            ON requests (model, created_at, id)
            WHERE state = 'pending'
              AND batch_id IS NULL
              AND template_id IS NOT NULL
              AND service_tier IS DISTINCT FROM 'background'
            "#,
            r#"
            CREATE INDEX idx_requests_active_sla_counts
            ON requests (batch_id, model)
            WHERE state IN ('pending', 'claimed', 'processing')
              AND template_id IS NOT NULL
              AND (
                  service_tier IS NULL
                  OR service_tier NOT IN ('priority', 'background')
              )
            "#,
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }

        MIGRATOR
            .run(&pool)
            .await
            .expect("the background migration must skip the pre-created column and indexes");

        for relation_name in [
            "idx_batches_background_active",
            "idx_requests_pending_background_batchless",
            "idx_requests_pending_background_batched",
            "idx_requests_pending_batchless_sla",
        ] {
            let relation_exists =
                sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
                    .bind(relation_name)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(
                relation_exists,
                "the pre-created relation {relation_name} must remain"
            );
        }

        // idx_requests_active_sla_counts is pre-created above like the others,
        // but a later migration (20260827) supersedes it with
        // idx_requests_active_batched_demand and drops it, so after the full
        // migration run it must be gone and its replacement present.
        for (relation_name, must_exist) in [
            ("idx_requests_active_sla_counts", false),
            ("idx_requests_active_non_priority_counts", false),
            ("idx_requests_active_batched_demand", true),
        ] {
            let relation_exists =
                sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
                    .bind(relation_name)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                relation_exists, must_exist,
                "{relation_name} presence after migrations must be {must_exist}"
            );
        }

        let migration_applied = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 20260722000000)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            migration_applied,
            "the migration must be recorded after skipping pre-created indexes"
        );
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
