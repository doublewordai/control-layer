use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::Context;
use fusillade::Storage;
use fusillade::manager::DaemonStorage;
use sqlx::{FromRow, PgPool};
use sqlx_pool_router::PoolProvider;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{Config, SharedConfig, config::RetentionConfig};

/// Internal batch metadata key storing artifact retention TTL in whole seconds.
pub(crate) const RETENTION_TTL_METADATA_KEY: &str = "dw_retention_ttl_seconds";

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SweepSummary {
    pub deleted_batches: usize,
    pub deleted_files: usize,
    pub purged_rows: u64,
}

#[derive(Debug, FromRow)]
struct DueBatchRow {
    id: Uuid,
    file_id: Option<Uuid>,
    output_file_id: Option<Uuid>,
    error_file_id: Option<Uuid>,
}

#[derive(Debug, FromRow)]
struct DueFileRow {
    id: Uuid,
}

/// Returns the configured default batch artifact TTL in seconds, if any.
pub(crate) fn default_batch_artifact_ttl_seconds(config: &Config) -> Option<i64> {
    config.background_services.retention.batch_artifacts_default_ttl_seconds()
}

/// Apply the configured default batch artifact TTL to a file metadata payload
/// when the request did not provide an explicit expiry.
pub(crate) fn apply_default_file_ttl(metadata: &mut fusillade::FileMetadata, config: &Config) {
    if metadata.expires_after_seconds.is_none() {
        metadata.expires_after_seconds = default_batch_artifact_ttl_seconds(config);
    }
}

/// Runs a single retention sweep pass.
pub(crate) async fn run_retention_pass<P: PoolProvider + Clone>(
    fusillade_pool: &PgPool,
    request_manager: &Arc<fusillade::PostgresRequestManager<P, fusillade::ReqwestHttpClient>>,
    retention: &RetentionConfig,
) -> anyhow::Result<SweepSummary> {
    let batch_size = retention.batch_size.max(1);

    let due_batches: Vec<DueBatchRow> = sqlx::query_as(
        r#"
        SELECT
            b.id,
            b.file_id,
            b.output_file_id,
            b.error_file_id
        FROM batches b
        WHERE b.deleted_at IS NULL
          AND COALESCE(b.completed_at, b.failed_at, b.cancelled_at) IS NOT NULL
          AND b.metadata IS NOT NULL
          AND (b.metadata ->> $1) IS NOT NULL
          AND (b.metadata ->> $1) ~ '^[0-9]+$'
          AND COALESCE(b.completed_at, b.failed_at, b.cancelled_at)
              + make_interval(secs => (b.metadata ->> $1)::BIGINT) <= NOW()
        ORDER BY COALESCE(b.completed_at, b.failed_at, b.cancelled_at) ASC, b.id ASC
        LIMIT $2
        "#,
    )
    .bind(RETENTION_TTL_METADATA_KEY)
    .bind(batch_size)
    .fetch_all(fusillade_pool)
    .await
    .context("query due batches for retention sweep")?;

    let due_files: Vec<DueFileRow> = sqlx::query_as(
        r#"
        SELECT f.id
        FROM files f
        LEFT JOIN batches bi ON bi.file_id = f.id AND bi.deleted_at IS NULL
        LEFT JOIN batches bo ON bo.output_file_id = f.id AND bo.deleted_at IS NULL
        LEFT JOIN batches be ON be.error_file_id = f.id AND be.deleted_at IS NULL
        WHERE f.deleted_at IS NULL
          AND f.expires_at IS NOT NULL
          AND f.expires_at <= NOW()
          AND bi.id IS NULL
          AND bo.id IS NULL
          AND be.id IS NULL
        ORDER BY f.expires_at ASC, f.id ASC
        LIMIT $1
        "#,
    )
    .bind(batch_size)
    .fetch_all(fusillade_pool)
    .await
    .context("query due standalone files for retention sweep")?;

    let mut summary = SweepSummary::default();
    let mut files_to_delete = HashSet::new();

    for batch in due_batches {
        request_manager
            .delete_batch(fusillade::BatchId(batch.id))
            .await
            .with_context(|| format!("soft-delete expired batch {}", batch.id))?;
        summary.deleted_batches += 1;

        files_to_delete.extend([batch.file_id, batch.output_file_id, batch.error_file_id].into_iter().flatten());
    }

    files_to_delete.extend(due_files.into_iter().map(|f| f.id));

    for file_id in files_to_delete {
        request_manager
            .delete_file(fusillade::FileId(file_id))
            .await
            .with_context(|| format!("soft-delete expired file {}", file_id))?;
        summary.deleted_files += 1;
    }

    if summary.deleted_batches > 0 || summary.deleted_files > 0 {
        summary.purged_rows = request_manager
            .purge_orphaned_rows(batch_size)
            .await
            .context("purge orphaned fusillade rows after retention sweep")?;
    }

    Ok(summary)
}

/// Periodic retention sweep loop.
pub(crate) async fn run_retention_loop<P: PoolProvider + Clone + Send + Sync + 'static>(
    fusillade_pool: PgPool,
    request_manager: Arc<fusillade::PostgresRequestManager<P, fusillade::ReqwestHttpClient>>,
    shared_config: SharedConfig,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    loop {
        let config = shared_config.snapshot();
        let retention = config.background_services.retention.clone();
        let interval = retention.sweep_interval;

        if retention.enabled {
            let summary = run_retention_pass(&fusillade_pool, &request_manager, &retention).await?;
            if summary.deleted_batches > 0 || summary.deleted_files > 0 || summary.purged_rows > 0 {
                tracing::info!(
                    deleted_batches = summary.deleted_batches,
                    deleted_files = summary.deleted_files,
                    purged_rows = summary.purged_rows,
                    "Retention sweep completed"
                );
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(normalize_interval(interval)) => {},
            _ = shutdown.cancelled() => return Ok(()),
        }
    }
}

fn normalize_interval(interval: Duration) -> Duration {
    if interval.is_zero() { Duration::from_secs(60) } else { interval }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use sqlx::PgPool;
    use sqlx::postgres::PgConnectOptions;

    use crate::{config::Config, test::utils::create_test_app_state_with_fusillade};

    async fn create_fusillade_pool(pool: &PgPool) -> PgPool {
        let base_opts: PgConnectOptions = pool.connect_options().as_ref().clone();
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .min_connections(0)
            .connect_with(base_opts.options([("search_path", "fusillade")]))
            .await
            .expect("Failed to create fusillade pool")
    }

    #[sqlx::test]
    async fn test_apply_default_file_ttl_respects_existing_expiry(pool: PgPool) {
        let mut config = Config::default();
        config.background_services.retention.batch_artifacts_default_ttl = Some(Duration::from_secs(600));

        let mut metadata = fusillade::FileMetadata {
            expires_after_seconds: Some(60),
            ..Default::default()
        };

        super::apply_default_file_ttl(&mut metadata, &config);
        assert_eq!(metadata.expires_after_seconds, Some(60));

        let mut metadata_without_expiry = fusillade::FileMetadata::default();
        super::apply_default_file_ttl(&mut metadata_without_expiry, &config);
        assert_eq!(metadata_without_expiry.expires_after_seconds, Some(600));

        drop(pool);
    }

    #[sqlx::test]
    async fn test_retention_sweep_deletes_due_batch_and_files(pool: PgPool) {
        let mut config = Config::default();
        config.background_services.retention.batch_size = 100;
        let state = create_test_app_state_with_fusillade(pool.clone(), config.clone()).await;
        let fusillade_pool = create_fusillade_pool(&pool).await;

        let input_file_id = Uuid::new_v4();
        let output_file_id = Uuid::new_v4();
        let error_file_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let template_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let metadata = serde_json::json!({
            super::RETENTION_TTL_METADATA_KEY: "0"
        });

        sqlx::query(
            r#"
            INSERT INTO fusillade.files (id, name, purpose, status, size_bytes, size_finalized, created_at, updated_at, expires_at)
            VALUES ($1, 'input.jsonl', 'batch', 'processed', 1, TRUE, NOW() - INTERVAL '2 hours', NOW(), NOW() - INTERVAL '1 hour'),
                   ($2, 'output.jsonl', 'batch_output', 'processed', 1, TRUE, NOW() - INTERVAL '2 hours', NOW(), NOW() - INTERVAL '1 hour'),
                   ($3, 'error.jsonl', 'batch_error', 'processed', 1, TRUE, NOW() - INTERVAL '2 hours', NOW(), NOW() - INTERVAL '1 hour')
            "#,
        )
        .bind(input_file_id)
        .bind(output_file_id)
        .bind(error_file_id)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method, created_at, updated_at)
            VALUES ($1, $2, 'test-model', 'test-key', 'http://test', '/v1/chat/completions', '{}', 'req-0', 'POST', NOW(), NOW())
            "#,
        )
        .bind(template_id)
        .bind(input_file_id)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO fusillade.batches (
                id, created_by, file_id, endpoint, completion_window, expires_at, created_at,
                total_requests, completed_at, metadata, output_file_id, error_file_id
            )
            VALUES (
                $1, $2, $3, '/v1/chat/completions', '24h', NOW() + INTERVAL '24 hours',
                NOW() - INTERVAL '2 hours', 1, NOW() - INTERVAL '1 hour', $4, $5, $6
            )
            "#,
        )
        .bind(batch_id)
        .bind(Uuid::new_v4().to_string())
        .bind(input_file_id)
        .bind(metadata)
        .bind(output_file_id)
        .bind(error_file_id)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO fusillade.requests (
                id, batch_id, template_id, endpoint, method, path, body, model, api_key,
                state, response_status, response_body, response_size, created_at, updated_at, completed_at
            )
            VALUES (
                $1, $2, $3, 'http://test', 'POST', '/v1/chat/completions', '{}', 'test-model', 'test-key',
                'completed', 200, '{}', 2, NOW() - INTERVAL '2 hours', NOW(), NOW() - INTERVAL '1 hour'
            )
            "#,
        )
        .bind(request_id)
        .bind(batch_id)
        .bind(template_id)
        .execute(&pool)
        .await
        .unwrap();

        let summary = super::run_retention_pass(&fusillade_pool, &state.request_manager, &config.background_services.retention)
            .await
            .unwrap();

        assert_eq!(summary.deleted_batches, 1);
        assert_eq!(summary.deleted_files, 3);
        assert!(summary.purged_rows >= 2);

        let deleted_at: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT deleted_at FROM fusillade.batches WHERE id = $1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(deleted_at.is_some());

        let file_delete_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM fusillade.files WHERE id = ANY($1) AND deleted_at IS NOT NULL")
                .bind(vec![input_file_id, output_file_id, error_file_id])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(file_delete_count, 3);

        let remaining_requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fusillade.requests WHERE batch_id = $1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining_requests, 0);

        let remaining_templates: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fusillade.request_templates WHERE file_id = $1")
            .bind(input_file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining_templates, 0);
    }
}
