//! Weekly batch-archive partition retirement.
//!
//! Deletes archived batch response content by dropping whole weekly
//! `batch_requests_archive` partitions once the week is past the retention
//! period measured from the week itself, so a batch's completions live a
//! fixed time from its creation, like its input file. Batch metadata rows are
//! never deleted. The API reports results files as expired from the
//! `expires_at` they were given at creation; nothing is stamped on the
//! batches at retirement, since any per-batch write at that point grows with
//! the week (a million rows in one statement, cancelled by the statement
//! timeout) and the week could never retire.

use crate::error::Result;
use crate::manager::RetainedResponseRetirementOutcome;
use crate::{PoolProvider, PostgresRequestManager};
use chrono::{Datelike, NaiveDate, Weekday};

use super::partition_retirement::{FamilySpec, retire};

fn monday_lower(lower: NaiveDate) -> bool {
    lower.weekday() == Weekday::Mon
}

fn weekly_child(week_start: NaiveDate) -> String {
    format!(
        "batch_requests_archive_y{}w{:02}",
        week_start.iso_week().year(),
        week_start.iso_week().week()
    )
}

const BATCH_ARCHIVE_FAMILY: FamilySpec = FamilySpec {
    parent: "batch_requests_archive",
    bucket_table: "batch_archive_buckets",
    bucket_key: "week_start",
    bounds_days: 7,
    valid_lower: monday_lower,
    canonical_child: weekly_child,
    orphan_fence_sql: r#"
        SELECT EXISTS (
            SELECT 1
            FROM batch_archive_buckets bucket
            WHERE (
                bucket.state = 'retiring'
                AND NOT EXISTS (
                    SELECT 1
                    FROM retention_partition_retirements journal
                    WHERE journal.parent_table = 'batch_requests_archive'
                      AND journal.partition_schema = bucket.partition_schema
                      AND journal.partition_table = bucket.partition_table
                      AND journal.partition_oid = bucket.partition_oid
                      AND journal.lower_bound = bucket.week_start
                      AND journal.upper_bound = bucket.week_start + 7
                      AND journal.completed_at IS NULL
                )
            ) OR (
                bucket.state = 'retired'
                AND NOT EXISTS (
                    SELECT 1
                    FROM retention_partition_retirements journal
                    WHERE journal.parent_table = 'batch_requests_archive'
                      AND journal.partition_schema = bucket.partition_schema
                      AND journal.partition_table = bucket.partition_table
                      AND journal.partition_oid = bucket.partition_oid
                      AND journal.lower_bound = bucket.week_start
                      AND journal.upper_bound = bucket.week_start + 7
                      AND journal.completed_at IS NOT NULL
                )
            )
        )
        "#,
    // A weekly bucket is eligible when the whole week is past the retention
    // horizon and no batch created in that week is still running: such a
    // batch may yet write results into the partition. A finished batch holds
    // nothing open, however late it finished; one retry no longer extends
    // the week for every batch in it. The guard reads only unfinished
    // batches, a few dozen rows under idx_batches_unfinished_created_at.
    //
    // The guard keys on the week a batch was *created* in, while archived
    // rows are partitioned by `batches.archive_bucket`. The two differ only
    // for a batch whose creation week had already been fenced when it
    // archived: archive_batch then places it in the current week. Such a
    // batch, once retried, does not hold that later week through this
    // guard. Retry bounds the gap instead: it share-locks the batch's own
    // archive_bucket row and refuses unless that week is active, and the
    // retry age cut-off (RetryAgeLimits) stops retries a day before the
    // creation week can drop. The later week drops at least a week after
    // the creation week, so a retried batch has that long to finish before
    // its archived completions could go. Not worth an expression index on
    // COALESCE(archive_bucket, creation week) for this edge.
    candidate_sql: r#"
        SELECT bucket.partition_schema,
               namespace.oid AS partition_schema_oid,
               parent.oid AS parent_oid,
               bucket.partition_table,
               bucket.partition_oid,
               bucket.week_start AS lower_bound,
               bucket.week_start + 7 AS upper_bound
        FROM batch_archive_buckets bucket
        JOIN pg_namespace namespace
          ON namespace.nspname = bucket.partition_schema
        JOIN pg_class parent
          ON parent.relnamespace = namespace.oid
         AND parent.relname = 'batch_requests_archive'
        WHERE bucket.state = 'active'
          AND (bucket.week_start + 7 + make_interval(days => $1))
                <= statement_timestamp() AT TIME ZONE 'UTC'
          AND NOT EXISTS (
              SELECT 1
              FROM batches b
              WHERE b.counts_frozen_at IS NULL
                AND b.deleted_at IS NULL
                AND b.created_at >= (bucket.week_start::timestamp AT TIME ZONE 'UTC')
                AND b.created_at < ((bucket.week_start + 7)::timestamp AT TIME ZONE 'UTC')
          )
        ORDER BY bucket.week_start
        LIMIT 1
        "#,
    candidate_binds_retention: true,
    // The candidate's unfinished-batch guard, re-run under the bucket lock.
    recheck_sql: Some(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM batches b
            WHERE b.counts_frozen_at IS NULL
              AND b.deleted_at IS NULL
              AND b.created_at >= ($1::date::timestamp AT TIME ZONE 'UTC')
              AND b.created_at < (($1::date + 7)::timestamp AT TIME ZONE 'UTC')
        )
        "#,
    ),
    completion_sql: None,
};

pub(super) async fn retire_expired_batch_archive_partition<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    select_new: bool,
    retention_days: i32,
) -> Result<RetainedResponseRetirementOutcome> {
    retire(
        manager,
        &BATCH_ARCHIVE_FAMILY,
        select_new,
        Some(retention_days),
    )
    .await
}
