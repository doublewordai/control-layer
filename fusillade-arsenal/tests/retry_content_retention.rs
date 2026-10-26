//! Batch creation and retry refuse input or results whose content week is
//! being, or has been, retired, and retry refuses a batch near the end of
//! its content retention (`RetryAgeLimits`).

use chrono::{Datelike, Duration, NaiveDate, Utc};
use fusillade_arsenal::batch::{BatchId, BatchInput, FileId, RequestTemplateInput};
use fusillade_arsenal::error::FusilladeError;
use fusillade_arsenal::request::RequestId;
use fusillade_arsenal::{
    PostgresRequestManager, PostgresStorageConfig, RetryAgeLimits, Storage, TestDbPools,
};
use sqlx::PgPool;
use uuid::Uuid;

async fn manager(pool: &PgPool, limits: RetryAgeLimits) -> PostgresRequestManager<TestDbPools> {
    PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    )
    .with_retry_age_limits(limits)
}

async fn upload(manager: &PostgresRequestManager<TestDbPools>) -> FileId {
    manager
        .create_file(
            "retention-input".to_string(),
            None,
            vec![RequestTemplateInput {
                custom_id: Some("retention-req".to_string()),
                endpoint: "https://example.invalid".to_string(),
                method: "POST".to_string(),
                path: "/v1/x".to_string(),
                body: "{}".to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            }],
        )
        .await
        .unwrap()
}

fn batch_input(file_id: FileId) -> BatchInput {
    BatchInput {
        file_id,
        endpoint: "/v1/chat/completions".to_string(),
        completion_window: "24h".to_string(),
        metadata: None,
        created_by: None,
        api_key_id: None,
        api_key: None,
        total_requests: None,
    }
}

/// A live batch whose only request has failed, ready to retry.
async fn failed_batch(pool: &PgPool, manager: &PostgresRequestManager<TestDbPools>) -> BatchId {
    let file_id = upload(manager).await;
    let batch = manager.create_batch(batch_input(file_id)).await.unwrap();
    sqlx::query(
        "UPDATE requests SET state = 'failed', failed_at = NOW(), error = 'boom' \
         WHERE batch_id = $1",
    )
    .bind(*batch.id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE batches SET failed_at = NOW(), counts_frozen_at = NOW(), failed_requests = 1 \
         WHERE id = $1",
    )
    .bind(*batch.id)
    .execute(pool)
    .await
    .unwrap();
    batch.id
}

async fn request_ids(pool: &PgPool, batch_id: BatchId) -> Vec<RequestId> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM requests WHERE batch_id = $1")
        .bind(*batch_id)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(RequestId)
        .collect()
}

/// Set the state of the template week a file's content was written to.
async fn set_input_week_state(pool: &PgPool, file_id: Uuid, state: &str) {
    let updated = sqlx::query(
        "UPDATE request_template_buckets SET state = $2 \
         WHERE week_start = (SELECT date_trunc('week', created_at AT TIME ZONE 'UTC')::date \
                             FROM files WHERE id = $1)",
    )
    .bind(file_id)
    .bind(state)
    .execute(pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(updated, 1);
}

async fn batch_file(pool: &PgPool, batch_id: BatchId) -> Uuid {
    sqlx::query_scalar("SELECT file_id FROM batches WHERE id = $1")
        .bind(*batch_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn assert_validation(result: Result<impl std::fmt::Debug, FusilladeError>, context: &str) {
    match result {
        Err(FusilladeError::ValidationError(_)) => {}
        other => panic!("{context}: expected a validation error, got {other:?}"),
    }
}

#[sqlx::test]
async fn batch_creation_requires_an_active_input_week(pool: PgPool) {
    let manager = manager(&pool, RetryAgeLimits::default()).await;
    let file_id = upload(&manager).await;

    for state in ["retiring", "retired"] {
        set_input_week_state(&pool, *file_id, state).await;
        assert_validation(
            manager.create_batch(batch_input(file_id)).await,
            &format!("input week {state}"),
        );
    }
    let batches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM batches WHERE file_id = $1")
        .bind(*file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(batches, 0, "a refused creation must leave no batch behind");

    set_input_week_state(&pool, *file_id, "active").await;
    manager.create_batch(batch_input(file_id)).await.unwrap();
}

#[sqlx::test]
async fn batch_retry_requires_an_active_input_week(pool: PgPool) {
    let manager = manager(&pool, RetryAgeLimits::default()).await;
    let batch_id = failed_batch(&pool, &manager).await;
    let file_id = batch_file(&pool, batch_id).await;

    for state in ["retiring", "retired"] {
        set_input_week_state(&pool, file_id, state).await;
        assert_validation(
            manager.retry_failed_requests_for_batch(batch_id).await,
            &format!("input week {state}"),
        );
    }

    set_input_week_state(&pool, file_id, "active").await;
    assert_eq!(
        manager
            .retry_failed_requests_for_batch(batch_id)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test]
async fn request_retry_requires_an_active_input_week(pool: PgPool) {
    let manager = manager(&pool, RetryAgeLimits::default()).await;
    let batch_id = failed_batch(&pool, &manager).await;
    let file_id = batch_file(&pool, batch_id).await;
    let ids = request_ids(&pool, batch_id).await;

    // Request retry reports per request, so an unavailable source shows up
    // as a validation error on each requested row.
    for state in ["retiring", "retired"] {
        set_input_week_state(&pool, file_id, state).await;
        let results = manager
            .retry_failed_requests(batch_id, ids.clone())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_validation(
            results.into_iter().next().unwrap(),
            &format!("input week {state}"),
        );
    }
    let state: String = sqlx::query_scalar("SELECT state FROM requests WHERE batch_id = $1")
        .bind(*batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "failed", "a refused retry must re-pend nothing");

    set_input_week_state(&pool, file_id, "active").await;
    let results = manager
        .retry_failed_requests(batch_id, ids.clone())
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_ok(), "{:?}", results[0]);
}

async fn set_batch_created_at(pool: &PgPool, batch_id: BatchId, age: &str) {
    sqlx::query(&format!(
        "UPDATE batches SET created_at = NOW() - INTERVAL '{age}' WHERE id = $1"
    ))
    .bind(*batch_id)
    .execute(pool)
    .await
    .unwrap();
}

/// With a 30-day results period, retries are accepted until the batch is 29
/// days old: one day before its archive week could drop.
#[sqlx::test]
async fn batch_retry_stops_a_day_before_results_retention_ends(pool: PgPool) {
    let manager = manager(
        &pool,
        RetryAgeLimits {
            batch_archive_days: Some(30),
            template_days: None,
        },
    )
    .await;

    let inside = failed_batch(&pool, &manager).await;
    set_batch_created_at(&pool, inside, "28 days 23 hours").await;
    assert_eq!(
        manager
            .retry_failed_requests_for_batch(inside)
            .await
            .unwrap(),
        1
    );

    let past = failed_batch(&pool, &manager).await;
    set_batch_created_at(&pool, past, "29 days 1 hour").await;
    assert_validation(
        manager.retry_failed_requests_for_batch(past).await,
        "batch past the cut-off",
    );
    let ids = request_ids(&pool, past).await;
    assert_validation(
        manager.retry_failed_requests(past, ids).await,
        "request retry past the cut-off",
    );
    let state: String = sqlx::query_scalar("SELECT state FROM requests WHERE batch_id = $1")
        .bind(*past)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "failed", "a refused retry must re-pend nothing");
}

/// Move a batch's input file to `age` ago, keeping its (possibly earlier)
/// template week registered and active so only the age decides.
async fn set_input_created_at(pool: &PgPool, batch_id: BatchId, age: &str) {
    let file_id = batch_file(pool, batch_id).await;
    let week: NaiveDate = sqlx::query_scalar(&format!(
        "SELECT date_trunc('week', (NOW() - INTERVAL '{age}') AT TIME ZONE 'UTC')::date"
    ))
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "UPDATE files SET created_at = NOW() - INTERVAL '{age}' WHERE id = $1"
    ))
    .bind(file_id)
    .execute(pool)
    .await
    .unwrap();
}

/// With a 10-day input period, retries are accepted until the input file is
/// 9 days old: one day before its template week could drop.
#[sqlx::test]
async fn batch_retry_stops_a_day_before_input_retention_ends(pool: PgPool) {
    let manager = manager(
        &pool,
        RetryAgeLimits {
            batch_archive_days: None,
            template_days: Some(10),
        },
    )
    .await;

    let inside = failed_batch(&pool, &manager).await;
    set_input_created_at(&pool, inside, "8 days 23 hours").await;
    assert_eq!(
        manager
            .retry_failed_requests_for_batch(inside)
            .await
            .unwrap(),
        1
    );

    let past = failed_batch(&pool, &manager).await;
    set_input_created_at(&pool, past, "9 days 1 hour").await;
    assert_validation(
        manager.retry_failed_requests_for_batch(past).await,
        "input past the cut-off",
    );
}

fn archive_child(week_start: NaiveDate) -> String {
    format!(
        "batch_requests_archive_y{}w{:02}",
        week_start.iso_week().year(),
        week_start.iso_week().week()
    )
}

/// Register a historical archive week (child, attach, registry row).
async fn ensure_archive_week(pool: &PgPool, week_start: NaiveDate) {
    let child = archive_child(week_start);
    sqlx::query(&format!(
        "CREATE TABLE {child} (LIKE batch_requests_archive INCLUDING ALL)"
    ))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "ALTER TABLE batch_requests_archive ATTACH PARTITION {child} \
         FOR VALUES FROM ('{week_start}') TO ('{}')",
        week_start + Duration::days(7)
    ))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO batch_archive_buckets (
             week_start, partition_schema, partition_table, partition_oid
         ) SELECT $1, current_schema(), $2, to_regclass($2)::oid",
    )
    .bind(week_start)
    .bind(&child)
    .execute(pool)
    .await
    .unwrap();
}

/// Retrying an archived batch whose archive week is fenced or dropped would
/// move back nothing and reset the batch over completions that are gone.
#[sqlx::test]
async fn archived_batch_retry_requires_an_active_archive_week(pool: PgPool) {
    let manager = manager(&pool, RetryAgeLimits::default()).await;
    let today = Utc::now().date_naive();
    let week =
        today - Duration::days(today.weekday().num_days_from_monday().into()) - Duration::days(70);
    ensure_archive_week(&pool, week).await;

    let batch_id = failed_batch(&pool, &manager).await;
    sqlx::query("UPDATE batches SET location = 'archive', archive_bucket = $2 WHERE id = $1")
        .bind(*batch_id)
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();

    for state in ["retiring", "retired"] {
        sqlx::query("UPDATE batch_archive_buckets SET state = $2 WHERE week_start = $1")
            .bind(week)
            .bind(state)
            .execute(&pool)
            .await
            .unwrap();
        assert_validation(
            manager.retry_failed_requests_for_batch(batch_id).await,
            &format!("archive week {state}"),
        );
        let frozen: bool =
            sqlx::query_scalar("SELECT counts_frozen_at IS NOT NULL FROM batches WHERE id = $1")
                .bind(*batch_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(frozen, "a refused retry must not reset the batch");
    }

    sqlx::query("UPDATE batch_archive_buckets SET state = 'active' WHERE week_start = $1")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        manager
            .retry_failed_requests_for_batch(batch_id)
            .await
            .unwrap(),
        1
    );
}
