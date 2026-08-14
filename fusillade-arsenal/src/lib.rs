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
    ArchiveOutcome, BackgroundClaimKind, DaemonStorage, ModelFilter, ModelFilterState,
    RetentionSweepCutoffs, RetentionSweepOutcome, RetentionSweepPolicy, Storage,
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
            "idx_requests_active_sla_counts",
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

    #[sqlx::test(migrations = false)]
    async fn retention_migration_routes_new_templates_without_copying_legacy_rows(
        pool: sqlx::PgPool,
    ) {
        const RETENTION_MIGRATION: i64 = 20260813000000;
        let baseline = sqlx::migrate::Migrator {
            migrations: Cow::Owned(
                MIGRATOR
                    .iter()
                    .filter(|migration| migration.version < RETENTION_MIGRATION)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: false,
            locking: true,
            no_tx: false,
        };
        baseline.run(&pool).await.unwrap();

        let file_id = uuid::Uuid::new_v4();
        let legacy_template_id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO files (id, name) VALUES ($1, 'legacy.jsonl')")
            .bind(file_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            r#"
            INSERT INTO request_templates (
                id, file_id, endpoint, method, path, body, model, api_key
            ) VALUES ($1, $2, '/v1/responses', 'POST', '/v1/responses',
                      '{"legacy":true}', 'test-model', 'legacy-key')
            "#,
        )
        .bind(legacy_template_id)
        .bind(file_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut prepared_connection = pool.acquire().await.unwrap();
        sqlx::query(
            r#"
            PREPARE insert_template_across_cutover(UUID, UUID) AS
            INSERT INTO request_templates (
                id, file_id, endpoint, method, path, body, model, api_key
            ) VALUES ($1, $2, '/v1/responses', 'POST', '/v1/responses',
                      '{"prepared":true}', 'test-model', 'prepared-key')
            "#,
        )
        .execute(&mut *prepared_connection)
        .await
        .unwrap();

        MIGRATOR.run(&mut *prepared_connection).await.unwrap();

        let legacy_visible: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM request_templates WHERE id = $1)")
                .bind(legacy_template_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(legacy_visible, "the cutover must preserve legacy reads");

        let legacy_stayed_in_place: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM request_templates_legacy WHERE id = $1)",
        )
        .bind(legacy_template_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            legacy_stayed_in_place,
            "the migration must rename the legacy table instead of copying it"
        );
        let legacy_generation_ready: bool = sqlx::query_scalar(
            "SELECT request_template_legacy_retirement_ready(INTERVAL '90 days')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !legacy_generation_ready,
            "the pre-cutover generation must remain guarded for the configured age"
        );

        let retained_template_id = uuid::Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO request_templates (
                id, file_id, endpoint, method, path, body, model, api_key
            ) VALUES ($1, $2, '/v1/responses', 'POST', '/v1/responses',
                      '{"retained":true}', 'test-model', 'retained-key')
            "#,
        )
        .bind(retained_template_id)
        .bind(file_id)
        .execute(&pool)
        .await
        .unwrap();

        let retained_table: String = sqlx::query_scalar(
            r#"
            SELECT tableoid::regclass::text
            FROM request_templates_retained
            WHERE id = $1
            "#,
        )
        .bind(retained_template_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            retained_table.starts_with("request_templates_retained_y"),
            "new content must route directly into a time partition, got {retained_table}"
        );

        let registry_stub: (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
            "SELECT body, retained_bucket FROM request_templates_legacy WHERE id = $1",
        )
        .bind(retained_template_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            registry_stub.0.is_empty() && registry_stub.1.is_some(),
            "the legacy table may receive only a content-free routing stub"
        );

        for weeks_ago in 1..=8_i32 {
            sqlx::query(
                "SELECT ensure_request_template_partition(NOW() - make_interval(weeks => $1))",
            )
            .bind(weeks_ago)
            .execute(&pool)
            .await
            .unwrap();
        }
        let plan: serde_json::Value = sqlx::query_scalar(
            "EXPLAIN (ANALYZE, COSTS OFF, FORMAT JSON) SELECT body FROM request_templates WHERE id = $1",
        )
        .bind(retained_template_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        fn executed_template_children(value: &serde_json::Value) -> usize {
            let this_node = value
                .as_object()
                .filter(|node| {
                    node.get("Relation Name")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|name| name.starts_with("request_templates_retained_y"))
                        && node
                            .get("Actual Loops")
                            .and_then(serde_json::Value::as_f64)
                            .is_some_and(|loops| loops > 0.0)
                })
                .is_some() as usize;
            this_node
                + value
                    .as_array()
                    .map(|items| items.iter().map(executed_template_children).sum())
                    .unwrap_or(0)
                + value
                    .as_object()
                    .map(|object| object.values().map(executed_template_children).sum())
                    .unwrap_or(0)
        }
        assert_eq!(
            executed_template_children(&plan),
            1,
            "an ID lookup must execute against exactly one retained partition"
        );
        let file_plan: serde_json::Value = sqlx::query_scalar(
            "EXPLAIN (ANALYZE, COSTS OFF, FORMAT JSON) SELECT line_number, body FROM request_templates_by_file WHERE file_id = $1 ORDER BY line_number",
        )
        .bind(file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            executed_template_children(&file_plan),
            1,
            "a file lookup must execute against only the owning retained partition"
        );
        fn template_child_loops(value: &serde_json::Value) -> usize {
            let this_node = value
                .as_object()
                .filter(|node| {
                    node.get("Relation Name")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|name| name.starts_with("request_templates_retained_y"))
                })
                .and_then(|node| node.get("Actual Loops"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0) as usize;
            this_node
                + value
                    .as_array()
                    .map(|items| items.iter().map(template_child_loops).sum())
                    .unwrap_or(0)
                + value
                    .as_object()
                    .map(|object| object.values().map(template_child_loops).sum())
                    .unwrap_or(0)
        }
        assert_eq!(
            template_child_loops(&file_plan),
            1,
            "a set-oriented file lookup must scan the retained child once, not once per row"
        );

        let prepared_template_id = uuid::Uuid::new_v4();
        let execute_prepared = format!(
            "EXECUTE insert_template_across_cutover('{}'::uuid, '{}'::uuid)",
            prepared_template_id, file_id
        );
        sqlx::query(&execute_prepared)
            .execute(&mut *prepared_connection)
            .await
            .expect("prepared statements must replan against the compatibility view");
        let prepared_retained: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM request_templates_retained WHERE id = $1)",
        )
        .bind(prepared_template_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(prepared_retained);

        let generated_template_id: uuid::Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO request_templates (
                file_id, endpoint, method, path, body, model, api_key
            ) VALUES ($1, '/v1/responses', 'POST', '/v1/responses',
                      '{}', 'test-model', 'generated-key')
            RETURNING id
            "#,
        )
        .bind(file_id)
        .fetch_one(&pool)
        .await
        .expect("view defaults must generate an identity");
        sqlx::query(
            r#"
            INSERT INTO requests (
                id, batch_id, template_id, model, state, retry_attempt,
                service_tier, created_by
            ) VALUES ($1, NULL, $2, 'test-model', 'pending', 0,
                      'background', 'migration-test')
            "#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(generated_template_id)
        .execute(&pool)
        .await
        .expect("the retained template stub must satisfy the existing FK");

        let update_error = sqlx::query(
            "UPDATE request_templates SET created_at = created_at + INTERVAL '1 day' WHERE id = $1",
        )
        .bind(generated_template_id)
        .execute(&pool)
        .await
        .expect_err("the partition routing timestamp must be immutable");
        assert_eq!(
            update_error
                .as_database_error()
                .and_then(|error| error.code())
                .as_deref(),
            Some("23000")
        );
    }

    #[sqlx::test(migrations = false)]
    async fn template_write_trigger_uses_its_relation_schema(pool: sqlx::PgPool) {
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

        sqlx::query("CREATE SCHEMA retention_scope")
            .execute(&pool)
            .await
            .unwrap();
        let options: PgConnectOptions = pool.connect_options().as_ref().clone();
        let scoped_pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.options([("search_path", "retention_scope")]))
            .await
            .unwrap();
        MIGRATOR.run(&scoped_pool).await.unwrap();

        let file_id = uuid::Uuid::new_v4();
        let template_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO retention_scope.files (id, name) VALUES ($1, 'schema-test.jsonl')",
        )
        .bind(file_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO retention_scope.request_templates (
                id, file_id, endpoint, method, path, body, model, api_key
            ) VALUES ($1, $2, '/v1/responses', 'POST', '/v1/responses',
                      '{}', 'test-model', 'schema-test-key')
            "#,
        )
        .bind(template_id)
        .bind(file_id)
        .execute(&pool)
        .await
        .expect("a schema-qualified view write must route to that schema's retained parent");

        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM retention_scope.request_templates_retained WHERE id = $1)",
        )
        .bind(template_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(retained);
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
