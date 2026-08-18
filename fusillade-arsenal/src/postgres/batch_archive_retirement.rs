//! Weekly batch-archive partition retirement.
//!
//! Deletes archived batch response content by dropping whole weekly
//! `batch_requests_archive` partitions once every batch in the week is past
//! its content-retention period. Batch metadata rows are never deleted:
//! completion atomically stamps `batches.retention_expired_at` so the API
//! reports expiry while the account-lifetime metadata survives.

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
    // A weekly bucket is eligible only when the whole week is past the
    // retention horizon AND no batch bucketed in it can still need its
    // content: every one is fully archived, frozen, and individually past
    // its own finalization-anchored retention period. Any live, split,
    // unfrozen, or younger batch fails the gate closed. The scan is over
    // batch metadata only.
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
              WHERE b.archive_bucket >= bucket.week_start
                AND b.archive_bucket < bucket.week_start + 7
                AND (
                    b.location <> 'archive'
                    OR b.counts_frozen_at IS NULL
                    OR (b.counts_frozen_at + make_interval(days => $1))
                          > statement_timestamp()
                )
          )
        ORDER BY bucket.week_start
        LIMIT 1
        "#,
    candidate_binds_retention: true,
    // Committed atomically with the journal/bucket completion: batch
    // metadata survives, stamped with the exact deletion timestamp so the
    // API reports expired content and the explicit-erasure purge never
    // treats these tombstones as its own.
    completion_sql: Some(
        "UPDATE batches SET retention_expired_at = $3 \
         WHERE archive_bucket >= $1 AND archive_bucket < $2 \
           AND retention_expired_at IS NULL",
    ),
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
