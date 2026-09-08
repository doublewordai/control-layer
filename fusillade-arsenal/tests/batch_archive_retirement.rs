use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use fusillade_arsenal::manager::RetainedResponseRetirementOutcome;
use fusillade_arsenal::{
    DaemonStorage, PostgresRequestManager, PostgresStorageConfig, TestDbPools,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn child_name(week_start: NaiveDate) -> String {
    format!(
        "batch_requests_archive_y{}w{:02}",
        week_start.iso_week().year(),
        week_start.iso_week().week()
    )
}

/// A Monday `weeks_back` whole weeks before the current UTC week.
fn monday(weeks_back: i64) -> NaiveDate {
    let today = Utc::now().date_naive();
    let this_monday = today - Duration::days(today.weekday().num_days_from_monday().into());
    this_monday - Duration::days(7 * weeks_back)
}

/// Mirror of the runway helper for historical weeks the runway function will
/// never create: standalone child, exact-bounds check, attach, registry row.
async fn ensure_week(pool: &PgPool, week_start: NaiveDate) {
    let child = child_name(week_start);
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

struct ArchivedBatch {
    batch_id: Uuid,
}

async fn archived_batch(
    pool: &PgPool,
    week_start: NaiveDate,
    frozen_at: DateTime<Utc>,
) -> ArchivedBatch {
    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO files (name, size_bytes, size_finalized, status, purpose) \
         VALUES ('bar-' || gen_random_uuid(), 0, TRUE, 'processed', 'batch') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let batch_id: Uuid = sqlx::query_scalar(
        "INSERT INTO batches (file_id, endpoint, completion_window, expires_at, \
                              location, archive_bucket, counts_frozen_at) \
         VALUES ($1, '/v1/x', '24h', $3 + INTERVAL '24 hours', 'archive', $2, $3) RETURNING id",
    )
    .bind(file_id)
    .bind(week_start)
    .bind(frozen_at)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO batch_requests_archive (id, batch_id, model, state, retry_attempt, \
                                             created_at, updated_at, completed_at, \
                                             response_status, response_body, response_size, \
                                             archive_bucket) \
         VALUES (gen_random_uuid(), $1, 'test-model', 'completed', 0, NOW(), NOW(), NOW(), \
                 200, '{\"archived\":true}', 18, $2)",
    )
    .bind(batch_id)
    .bind(week_start)
    .execute(pool)
    .await
    .unwrap();
    ArchivedBatch { batch_id }
}

async fn manager(pool: &PgPool) -> PostgresRequestManager<TestDbPools> {
    let maintenance_pool = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    )
    .with_retained_response_fence_seconds(Some(3_600))
    .with_partition_maintenance_pool(maintenance_pool)
    .unwrap()
    .attest_partition_maintenance_pool()
    .await
    .unwrap()
}

#[sqlx::test]
async fn an_expired_week_retires_and_stamps_batch_metadata(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;

    let outcome = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(true, 30)
        .await
        .unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::Retired);

    let child_exists: Option<i64> = sqlx::query_scalar("SELECT to_regclass($1)::oid::bigint")
        .bind(child_name(week))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(child_exists, None, "the weekly partition must be dropped");

    let (state, journal_done): (String, bool) = sqlx::query_as(
        "SELECT bucket.state, journal.completed_at IS NOT NULL \
         FROM batch_archive_buckets bucket \
         JOIN retention_partition_retirements journal \
           ON journal.parent_table = 'batch_requests_archive' \
          AND journal.lower_bound = bucket.week_start \
         WHERE bucket.week_start = $1",
    )
    .bind(week)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "retired");
    assert!(journal_done);

    // The batch metadata row survives, stamped with the deletion timestamp.
    let (expired_at, journal_completed): (Option<DateTime<Utc>>, DateTime<Utc>) = sqlx::query_as(
        "SELECT b.retention_expired_at, journal.completed_at \
             FROM batches b, retention_partition_retirements journal \
             WHERE b.id = $1 AND journal.parent_table = 'batch_requests_archive' \
               AND journal.lower_bound = $2",
    )
    .bind(batch.batch_id)
    .bind(week)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(expired_at, Some(journal_completed));

    let further = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(true, 30)
        .await
        .unwrap();
    assert_eq!(further, RetainedResponseRetirementOutcome::NoCandidate);
}

#[sqlx::test]
async fn a_batch_inside_its_retention_period_blocks_the_week(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    // A second batch in the same week finalized recently.
    archived_batch(&pool, week, Utc::now() - Duration::days(1)).await;

    let outcome = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(true, 30)
        .await
        .unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::NoCandidate);
    let bucket_state: String =
        sqlx::query_scalar("SELECT state FROM batch_archive_buckets WHERE week_start = $1")
            .bind(week)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(bucket_state, "active");
}

#[sqlx::test]
async fn live_split_or_unfrozen_batches_fail_the_gate_closed(pool: PgPool) {
    for (index, (location, frozen)) in [
        ("live", Some(Utc::now() - Duration::days(60))),
        ("split", Some(Utc::now() - Duration::days(60))),
        ("archive", None),
    ]
    .into_iter()
    .enumerate()
    {
        let week = monday(20 + index as i64);
        ensure_week(&pool, week).await;
        let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
        sqlx::query("UPDATE batches SET location = $2, counts_frozen_at = $3 WHERE id = $1")
            .bind(batch.batch_id)
            .bind(location)
            .bind(frozen)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = manager(&pool)
            .await
            .retire_expired_batch_archive_partition(true, 30)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            RetainedResponseRetirementOutcome::NoCandidate,
            "location={location} frozen={frozen:?} must block retirement"
        );
        sqlx::query(&format!(
            "ALTER TABLE batch_requests_archive DETACH PARTITION {}",
            child_name(week)
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(&format!("DROP TABLE {}", child_name(week)))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM batch_archive_buckets WHERE week_start = $1")
            .bind(week)
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[sqlx::test]
async fn recovery_completes_a_pending_journal_without_the_selection_flag(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO retention_partition_retirements (
             parent_table, partition_table, partition_oid,
             partition_schema, partition_schema_oid, parent_oid,
             lower_bound, upper_bound
         )
         SELECT 'batch_requests_archive', bucket.partition_table, bucket.partition_oid,
                bucket.partition_schema, namespace.oid,
                'batch_requests_archive'::regclass, $1, $1 + 7
         FROM batch_archive_buckets bucket
         JOIN pg_namespace namespace ON namespace.nspname = bucket.partition_schema
         WHERE bucket.week_start = $1",
    )
    .bind(week)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE batch_archive_buckets \
         SET state = 'retiring', state_changed_at = statement_timestamp() \
         WHERE week_start = $1",
    )
    .bind(week)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Recovery ignores both the selection flag and the retention period.
    let outcome = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(false, 0)
        .await
        .unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::Retired);
    let child_exists: Option<i64> = sqlx::query_scalar("SELECT to_regclass($1)::oid::bigint")
        .bind(child_name(week))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(child_exists, None);
}

#[sqlx::test]
async fn a_renamed_pending_child_fails_closed(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO retention_partition_retirements (
             parent_table, partition_table, partition_oid,
             partition_schema, partition_schema_oid, parent_oid,
             lower_bound, upper_bound
         )
         SELECT 'batch_requests_archive', bucket.partition_table, bucket.partition_oid,
                bucket.partition_schema, namespace.oid,
                'batch_requests_archive'::regclass, $1, $1 + 7
         FROM batch_archive_buckets bucket
         JOIN pg_namespace namespace ON namespace.nspname = bucket.partition_schema
         WHERE bucket.week_start = $1",
    )
    .bind(week)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE batch_archive_buckets SET state = 'retiring' WHERE week_start = $1")
        .bind(week)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let child = child_name(week);
    sqlx::query(&format!("ALTER TABLE {child} RENAME TO {child}_renamed"))
        .execute(&pool)
        .await
        .unwrap();

    let error = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(false, 0)
        .await
        .expect_err("a renamed relation must never be dropped");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent"
    );
    let renamed_exists: Option<i64> = sqlx::query_scalar("SELECT to_regclass($1)::oid::bigint")
        .bind(format!("{child}_renamed"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        renamed_exists.is_some(),
        "the renamed relation must survive"
    );
}

#[sqlx::test]
async fn stamping_never_regresses_an_earlier_expiry(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    // Truncate to PostgreSQL's microsecond resolution so the round-tripped
    // stamp compares exactly equal.
    let earlier = chrono::SubsecRound::trunc_subsecs(Utc::now() - Duration::days(5), 6);
    sqlx::query("UPDATE batches SET retention_expired_at = $2 WHERE id = $1")
        .bind(batch.batch_id)
        .bind(earlier)
        .execute(&pool)
        .await
        .unwrap();

    let outcome = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(true, 30)
        .await
        .unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::Retired);
    let stamped: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT retention_expired_at FROM batches WHERE id = $1")
            .bind(batch.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stamped,
        Some(earlier),
        "an existing stamp is never rewritten"
    );
}
