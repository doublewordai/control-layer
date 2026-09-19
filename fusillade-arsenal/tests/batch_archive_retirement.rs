use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use fusillade_arsenal::batch::BatchId;
use fusillade_arsenal::error::FusilladeError;
use fusillade_arsenal::manager::RetainedResponseRetirementOutcome;
use fusillade_arsenal::{
    DaemonStorage, PostgresRequestManager, PostgresStorageConfig, Storage, TestDbPools,
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

/// Add a `failed` archive row for a batch in a week. Retry only re-pends
/// `failed`/`canceled` archived rows, so this is the load-bearing fixture
/// element for exercising the retry archive-move-back path.
async fn add_failed_archive_row(pool: &PgPool, batch_id: Uuid, week: NaiveDate) {
    sqlx::query(
        "INSERT INTO batch_requests_archive (id, batch_id, model, state, retry_attempt, \
         created_at, updated_at, failed_at, error, response_status, response_body, \
         response_size, archive_bucket) \
         VALUES (gen_random_uuid(), $1, 'test-model', 'failed', 0, NOW(), NOW(), NOW(), \
                 'transient failure', NULL, NULL, 0, $2)",
    )
    .bind(batch_id)
    .bind(week)
    .execute(pool)
    .await
    .unwrap();
}

/// Synthesize the post-`claim()` crash-recovery DB state: an unfinished
/// `retention_partition_retirements` journal row plus `batch_archive_buckets
/// .state = 'retiring'`, modeling a `claim()` that committed its fence but
/// whose `finish()` never ran (lease since lapsed). `claim()`'s recovery arm
/// resumes this journal before any candidate selection, so a later
/// `retire(false, 0)` proceeds straight to `detach_and_finish()`.
async fn pre_fence_retiring_journal(pool: &PgPool, week: NaiveDate) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO retention_partition_retirements (\
             parent_table, partition_table, partition_oid, partition_schema, \
             partition_schema_oid, parent_oid, lower_bound, upper_bound\
         ) \
         SELECT 'batch_requests_archive', bucket.partition_table, bucket.partition_oid, \
                bucket.partition_schema, namespace.oid, \
                'batch_requests_archive'::regclass, $1, $1 + 7 \
         FROM batch_archive_buckets bucket \
         JOIN pg_namespace namespace ON namespace.nspname = bucket.partition_schema \
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
}

async fn batch_routing(pool: &PgPool, batch_id: Uuid) -> (String, Option<DateTime<Utc>>) {
    sqlx::query_as("SELECT location, counts_frozen_at FROM batches WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn archive_row_count(pool: &PgPool, week: NaiveDate, batch_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM batch_requests_archive \
         WHERE archive_bucket = $1 AND batch_id = $2",
    )
    .bind(week)
    .bind(batch_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn live_row_count(pool: &PgPool, batch_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*)::bigint FROM requests WHERE batch_id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// PRIMARY FIX: a retry on a batch whose weekly archive partition is fenced
/// for retirement (`state='retiring'`) is refused before the archive-move-back.
/// The batch stays `archive`+frozen (no un-freeze, no `location='split'`),
/// the archived `failed` AND `completed` rows stay in the partition, and no
/// row is re-pended to the live `requests` table — so the later `DROP TABLE`
/// cannot strand the `completed` archive rows out from under the retirement.
/// The outcome is a distinct `Err(RetryBlockedByArchiveFence)`, never the
/// `Ok(0)` that the HTTP layer would map to the "nothing to retry" `400`.
#[sqlx::test]
async fn retry_on_a_retiring_week_batch_is_blocked_and_leaves_the_archive_intact(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    add_failed_archive_row(&pool, batch.batch_id, week).await;
    // Fence the week (model a committed `claim()` whose `finish()` has not
    // run yet). The batch has a retriable `failed` archived row, so an
    // unguarded retry would un-freeze it and strand the `completed` row.
    sqlx::query("UPDATE batch_archive_buckets SET state = 'retiring' WHERE week_start = $1")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();

    let error = manager(&pool)
        .await
        .retry_failed_requests_for_batch(BatchId(batch.batch_id))
        .await
        .expect_err("a fenced-week retry must be refused before the archive-move-back");
    assert!(
        matches!(error, FusilladeError::RetryBlockedByArchiveFence),
        "fenced-week retry must signal the distinct fence error, got {error:?}"
    );

    // The batch is NOT un-frozen — `location` stays `'archive'` and
    // `counts_frozen_at` keeps its freeze stamp.
    let (location, frozen_at) = batch_routing(&pool, batch.batch_id).await;
    assert_eq!(location, "archive");
    assert!(frozen_at.is_some(), "retry must not clear counts_frozen_at");

    // Both archive rows survive (the `failed` row is not re-pended, the
    // `completed` row is not stranded by a premature move).
    assert_eq!(
        archive_row_count(&pool, week, batch.batch_id).await,
        2,
        "archive rows must be untouched"
    );
    // Nothing was re-pended to the live `requests` table.
    assert_eq!(
        live_row_count(&pool, batch.batch_id).await,
        0,
        "no row may be re-pended when the retry is fenced out"
    );
}

/// REGRESSION for the primary fix: a retry on an archived/frozen batch in an
/// `active` week still proceeds — re-pends the `failed` archived row to the
/// live `requests` table, deletes it from the archive, un-freezes the batch
/// to `location='split'`, and leaves the `completed` archived row in place.
/// The fence guard must not turn every archived-batch retry into a no-op.
#[sqlx::test]
async fn retry_on_an_active_week_archived_batch_moves_rows_and_unfreezes(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    add_failed_archive_row(&pool, batch.batch_id, week).await;

    let retried = manager(&pool)
        .await
        .retry_failed_requests_for_batch(BatchId(batch.batch_id))
        .await
        .expect("an active-week retry must proceed");
    assert_eq!(retried, 1, "the one failed archive row is re-pended");

    let (location, frozen_at) = batch_routing(&pool, batch.batch_id).await;
    assert_eq!(
        location, "split",
        "an archived batch with a leftover completed row becomes split"
    );
    assert!(
        frozen_at.is_none(),
        "a successful retry un-freezes the batch"
    );

    // The `failed` row was deleted from the archive; the `completed` row stays.
    assert_eq!(
        archive_row_count(&pool, week, batch.batch_id).await,
        1,
        "only the completed archive row remains"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM requests WHERE batch_id = $1 AND state = 'pending'",
    )
    .bind(batch.batch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        pending, 1,
        "the failed archive row was re-pended as pending"
    );
}

/// API-CONTRACT COROLLARY: once the retirement has completed (`state='retired'`,
/// partition gone) a subsequent retry is an ordinary no-op `Ok(0)` — the
/// archive prunes to empty and the `location`-reset `WHERE` is false — never
/// the `RetryBlockedByArchiveFence` error. This is the distinction that keeps
/// a fenced-but-genuinely-retriable batch returning retry-later `503` while a
/// retired batch whose rows were erased by retention returns the accurate
/// `400 "nothing to retry"` (it does NOT 503 forever). Also exercises the
/// `finish()` defense-in-depth re-check passing on a normally retiring week.
#[sqlx::test]
async fn retry_on_a_retired_week_batch_is_a_noop_returning_zero(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    let frozen_before: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT counts_frozen_at FROM batches WHERE id = $1")
            .bind(batch.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    // Run the full retirement: fence → detach → re-check → drop → stamp
    // `state='retired'`. The batch is archive/frozen/past-retention so the
    // re-check passes and the partition is dropped.
    let outcome = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(true, 30)
        .await
        .unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::Retired);

    let bucket_state: String =
        sqlx::query_scalar("SELECT state FROM batch_archive_buckets WHERE week_start = $1")
            .bind(week)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(bucket_state, "retired");

    // A retry now is an ordinary no-op — NOT the fence error.
    let retried = manager(&pool)
        .await
        .retry_failed_requests_for_batch(BatchId(batch.batch_id))
        .await
        .expect("a retry after the retirement completes is a no-op, not an error");
    assert_eq!(
        retried, 0,
        "the dropped partition prunes to empty; nothing to re-pend"
    );

    // The batch is never un-frozen: `location` stays `'archive'` and the
    // freeze stamp survives (only `retention_expired_at` was stamped).
    let (location, frozen_after) = batch_routing(&pool, batch.batch_id).await;
    assert_eq!(location, "archive");
    assert_eq!(
        frozen_after, frozen_before,
        "a post-retirement no-op retry must not disturb the freeze stamp"
    );
}

/// DEFENSE-IN-DEPTH: `finish()` re-verifies the gate predicate inside the
/// DROP transaction. Simulate a writer that un-froze a fenced-week batch
/// (the state `retry_failed_requests_for_batch` would have left before the
/// primary fix) and assert the re-check refuses to `DROP TABLE`: the detached
/// child survives, the journal stays unfinished, and the bucket stays
/// `'retiring'` so a later tick retries (or awaits an operator). The primary
/// guard lives in the writer; this is the safety net for a future writer that
/// forgets the `FOR SHARE` on the bucket row.
#[sqlx::test]
async fn finish_refuses_to_drop_a_fenced_week_partition_once_a_batch_is_unfrozen(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    pre_fence_retiring_journal(&pool, week).await;
    // Simulate the unguarded writer regressing the gate: a fenced-week batch
    // flipped to `location='split'` and `counts_frozen_at = NULL`.
    sqlx::query(
        "UPDATE batches SET location = 'split', counts_frozen_at = NULL, \
         completed_requests = 0, failed_requests = 1 WHERE id = $1",
    )
    .bind(batch.batch_id)
    .execute(&pool)
    .await
    .unwrap();

    let error = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(false, 30)
        .await
        .expect_err("finish() must refuse to drop a partition whose batch regressed the gate");
    assert_eq!(
        error.to_string(),
        "Retained response partition retirement identity is inconsistent",
        "the defense-in-depth re-check fails closed as an identity mismatch"
    );

    // The child relation survives (still physically present — `DROP TABLE`
    // was refused), the journal is still unfinished, and the bucket is still
    // `'retiring'` so a later tick retries once the batch re-freezes.
    let child_exists: Option<i64> = sqlx::query_scalar("SELECT to_regclass($1)::oid::bigint")
        .bind(child_name(week))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(child_exists.is_some(), "the partition must NOT be dropped");

    let (bucket_state, journal_done): (String, bool) = sqlx::query_as(
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
    assert_eq!(
        bucket_state, "retiring",
        "the bucket must stay fenced for retry"
    );
    assert!(
        !journal_done,
        "the journal must stay unfinished for a later tick"
    );

    // The batch row is untouched by the refused drop (still split/unfrozen).
    let (location, frozen_at) = batch_routing(&pool, batch.batch_id).await;
    assert_eq!(location, "split");
    assert!(frozen_at.is_none());
}

/// REGRESSION for the defense-in-depth re-check: a pre-fenced recovery journal
/// for an archive/frozen/past-retention batch still retires and drops through
/// the re-check — the safety net does not block the normal path. (Mirrors
/// `recovery_completes_a_pending_journal_without_the_selection_flag` but with
/// an explicit `retention_days` bind so the re-check's retention clause is
/// exercised, not just the location/freeze clauses.)
#[sqlx::test]
async fn finish_drops_through_the_recheck_when_the_gate_still_holds(pool: PgPool) {
    let week = monday(10);
    ensure_week(&pool, week).await;
    let batch = archived_batch(&pool, week, Utc::now() - Duration::days(60)).await;
    pre_fence_retiring_journal(&pool, week).await;

    let outcome = manager(&pool)
        .await
        .retire_expired_batch_archive_partition(false, 30)
        .await
        .unwrap();
    assert_eq!(outcome, RetainedResponseRetirementOutcome::Retired);

    let child_exists: Option<i64> = sqlx::query_scalar("SELECT to_regclass($1)::oid::bigint")
        .bind(child_name(week))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        child_exists, None,
        "the partition must be dropped when the gate holds"
    );

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

    let expired: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT retention_expired_at FROM batches WHERE id = $1")
            .bind(batch.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(expired.is_some(), "the batch metadata is stamped retired");
}
