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
                              location, archive_bucket, counts_frozen_at, created_at) \
         VALUES ($1, '/v1/x', '24h', $3 + INTERVAL '24 hours', 'archive', $2, $3, \
                 ($2::timestamp AT TIME ZONE 'UTC') + INTERVAL '1 day') RETURNING id",
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
async fn an_expired_week_retires_without_touching_batch_metadata(pool: PgPool) {
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

    // The batch metadata row survives untouched: results files report
    // expiry from their own expires_at, and a per-batch write here would
    // grow with the week.
    let expired_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT retention_expired_at FROM batches WHERE id = $1")
            .bind(batch.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(expired_at, None);

    let further = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(true, 30)
        .await
        .unwrap();
    assert_eq!(further, RetainedResponseRetirementOutcome::NoCandidate);
}

/// Retention is measured from the week, not from when each batch finished:
/// one batch retried weeks later must not keep every other batch's
/// completions in the week alive.
#[sqlx::test]
async fn a_late_finish_does_not_hold_the_week(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    // A second batch in the same week finalized yesterday.
    archived_batch(&pool, week, Utc::now() - Duration::days(1)).await;

    let outcome = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(true, 30)
        .await
        .unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::Retired);
}

/// The week is not yet past `week end + retention days`.
#[sqlx::test]
async fn a_young_week_is_not_a_candidate(pool: PgPool) {
    let week = monday(4);
    ensure_week(&pool, week).await;
    archived_batch(&pool, week, Utc::now() - Duration::days(20)).await;

    let outcome = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(true, 30)
        .await
        .unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::NoCandidate);
}

/// Only a batch that is still running, and was created in the week, holds
/// it open: it may yet write results into the partition. A finished batch
/// holds nothing open whatever its location, and a running batch from
/// another week is not this week's concern.
#[sqlx::test]
async fn only_an_unfinished_batch_created_in_the_week_blocks(pool: PgPool) {
    let frozen = Some(Utc::now() - Duration::days(60));
    for (index, (location, frozen, created_in_week, expected)) in [
        (
            "live",
            frozen,
            true,
            RetainedResponseRetirementOutcome::Retired,
        ),
        (
            "split",
            frozen,
            true,
            RetainedResponseRetirementOutcome::Retired,
        ),
        (
            "archive",
            None,
            true,
            RetainedResponseRetirementOutcome::NoCandidate,
        ),
        (
            "archive",
            None,
            false,
            RetainedResponseRetirementOutcome::Retired,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let week = monday(20 + index as i64);
        ensure_week(&pool, week).await;
        let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
        sqlx::query(
            "UPDATE batches SET location = $2, counts_frozen_at = $3, \
                 created_at = CASE WHEN $4 THEN created_at ELSE NOW() END \
             WHERE id = $1",
        )
        .bind(batch.batch_id)
        .bind(location)
        .bind(frozen)
        .bind(created_in_week)
        .execute(&pool)
        .await
        .unwrap();

        let outcome = manager(&pool)
            .await
            .retire_expired_batch_archive_partition(true, 30)
            .await
            .unwrap();
        assert_eq!(
            outcome, expected,
            "location={location} frozen={frozen:?} created_in_week={created_in_week}"
        );
        if expected == RetainedResponseRetirementOutcome::Retired {
            continue;
        }
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

/// The candidate query runs before the week's registry row is locked. A
/// writer that share-locked the row (as batch creation and retry do) and
/// commits a running batch while the claim waits for that lock must still
/// stop the week from being fenced: the claim re-checks under the lock.
#[sqlx::test]
async fn a_batch_committed_while_the_claim_waits_holds_the_week(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;

    let mut writer = pool.begin().await.unwrap();
    let state: String = sqlx::query_scalar(
        "SELECT state FROM batch_archive_buckets WHERE week_start = $1 FOR SHARE",
    )
    .bind(week)
    .fetch_one(&mut *writer)
    .await
    .unwrap();
    assert_eq!(state, "active");
    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO files (name, size_bytes, size_finalized, status, purpose) \
         VALUES ('race-' || gen_random_uuid(), 0, TRUE, 'processed', 'batch') RETURNING id",
    )
    .fetch_one(&mut *writer)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO batches (file_id, endpoint, completion_window, expires_at, created_at) \
         VALUES ($1, '/v1/x', '24h', NOW() + INTERVAL '24 hours', \
                 ($2::timestamp AT TIME ZONE 'UTC') + INTERVAL '2 days')",
    )
    .bind(file_id)
    .bind(week)
    .execute(&mut *writer)
    .await
    .unwrap();

    let manager = manager(&pool).await;
    let retirement = tokio::spawn(async move {
        manager
            .retire_expired_batch_archive_partition(true, 30)
            .await
    });

    // Poll until the claim is blocked on the registry row, well inside its
    // 5 s lock timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND pid <> pg_backend_pid()
                   AND wait_event_type = 'Lock'
                   AND query LIKE '%batch_archive_buckets%FOR UPDATE%'
             )",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the claim never waited for the week's registry row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    writer.commit().await.unwrap();

    let outcome = retirement.await.unwrap().unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::NoCandidate);
    let (state, journaled): (String, bool) = sqlx::query_as(
        "SELECT bucket.state, EXISTS ( \
             SELECT 1 FROM retention_partition_retirements journal \
             WHERE journal.parent_table = 'batch_requests_archive' \
               AND journal.lower_bound = bucket.week_start) \
         FROM batch_archive_buckets bucket WHERE bucket.week_start = $1",
    )
    .bind(week)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "active", "the week must not be fenced");
    assert!(!journaled, "no retirement may be journaled");
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
