//! Weekly generation-2 template retirement and the file-content expiry that
//! unpins it.
//!
//! Request input payloads are deleted by dropping whole weekly
//! `request_templates_g2` partitions. A week is eligible only when it is past
//! the retention horizon AND no live (undeleted) file still owns templates in
//! it — file metadata rows are never deleted by this lifecycle; scheduled
//! expiry only tombstones them once every referencing batch's content is
//! itself expired or erased.

use crate::error::Result;
use crate::manager::RetainedResponseRetirementOutcome;
use crate::{PoolProvider, PostgresRequestManager};
use chrono::{Datelike, NaiveDate, Weekday};

use super::partition_retirement::{FamilySpec, failed, retire};

fn monday_lower(lower: NaiveDate) -> bool {
    lower.weekday() == Weekday::Mon
}

fn weekly_child(week_start: NaiveDate) -> String {
    format!(
        "request_templates_g2_y{}w{:02}",
        week_start.iso_week().year(),
        week_start.iso_week().week()
    )
}

const TEMPLATE_FAMILY: FamilySpec = FamilySpec {
    parent: "request_templates_g2",
    bucket_table: "request_template_buckets",
    bucket_key: "week_start",
    bounds_days: 7,
    valid_lower: monday_lower,
    canonical_child: weekly_child,
    orphan_fence_sql: r#"
        SELECT EXISTS (
            SELECT 1
            FROM request_template_buckets bucket
            WHERE (
                bucket.state = 'retiring'
                AND NOT EXISTS (
                    SELECT 1
                    FROM retention_partition_retirements journal
                    WHERE journal.parent_table = 'request_templates_g2'
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
                    WHERE journal.parent_table = 'request_templates_g2'
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
    // Reference-gated eligibility: the week must be past the horizon, no
    // LIVE file may still own templates in it, and any row without a
    // resolvable owning file fails the gate closed (nothing unmanaged is
    // ever dropped implicitly). The scans touch one candidate partition and
    // metadata tables only.
    candidate_sql: r#"
        SELECT bucket.partition_schema,
               namespace.oid AS partition_schema_oid,
               parent.oid AS parent_oid,
               bucket.partition_table,
               bucket.partition_oid,
               bucket.week_start AS lower_bound,
               bucket.week_start + 7 AS upper_bound
        FROM request_template_buckets bucket
        JOIN pg_namespace namespace
          ON namespace.nspname = bucket.partition_schema
        JOIN pg_class parent
          ON parent.relnamespace = namespace.oid
         AND parent.relname = 'request_templates_g2'
        WHERE bucket.state = 'active'
          AND (bucket.week_start + 7 + make_interval(days => $1))
                <= statement_timestamp() AT TIME ZONE 'UTC'
          AND NOT EXISTS (
              SELECT 1
              FROM request_templates_g2 template
              JOIN files f ON f.id = template.file_id
              WHERE template.created_on >= bucket.week_start
                AND template.created_on < bucket.week_start + 7
                AND f.deleted_at IS NULL
          )
          AND NOT EXISTS (
              SELECT 1
              FROM request_templates_g2 template
              WHERE template.created_on >= bucket.week_start
                AND template.created_on < bucket.week_start + 7
                AND (
                    template.file_id IS NULL
                    OR NOT EXISTS (
                        SELECT 1 FROM files f WHERE f.id = template.file_id
                    )
                )
          )
        ORDER BY bucket.week_start
        LIMIT 1
        "#,
    candidate_binds_retention: true,
    completion_sql: None,
};

pub(super) async fn retire_expired_template_partition<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    select_new: bool,
    retention_days: i32,
) -> Result<RetainedResponseRetirementOutcome> {
    retire(manager, &TEMPLATE_FAMILY, select_new, Some(retention_days)).await
}

/// Tombstone input files whose content has aged out: the file row survives
/// with `retention_expired_at` set, downloads and new batch creation fail,
/// and its template window becomes droppable. Runs in bounded chunks; a file
/// is eligible only when every batch that referenced it has already had its
/// own content expired or explicitly erased.
pub(super) async fn expire_file_content<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    retention_days: i32,
    batch_size: i64,
) -> Result<u64> {
    if retention_days < 1 || batch_size < 1 {
        return Ok(0);
    }
    let mut transaction = manager.begin_write().await.map_err(|_| failed())?;
    let expired = sqlx::query(
        r#"
        WITH candidates AS MATERIALIZED (
            SELECT id
            FROM files
            WHERE purpose = 'batch'
              AND deleted_at IS NULL
              AND created_at + make_interval(days => $1) <= statement_timestamp()
              AND NOT EXISTS (
                  SELECT 1 FROM batches b
                  WHERE b.file_id = files.id
                    AND b.deleted_at IS NULL
                    AND b.retention_expired_at IS NULL
              )
            ORDER BY created_at, id
            LIMIT $2
            FOR UPDATE SKIP LOCKED
        )
        UPDATE files
        SET deleted_at = statement_timestamp(),
            retention_expired_at = statement_timestamp(),
            status = 'expired'
        FROM candidates
        WHERE files.id = candidates.id
          AND files.deleted_at IS NULL
        "#,
    )
    .bind(retention_days)
    .bind(batch_size)
    .execute(&mut *transaction)
    .await
    .map_err(|_| failed())?
    .rows_affected();
    transaction.commit().await.map_err(|_| failed())?;
    Ok(expired)
}

/// Bounded removal of location-oracle rows whose weekly bucket has been
/// physically retired. Templates need no resurrection fences: their ids are
/// never re-targeted by late writers.
pub(super) async fn cleanup_retired_template_routes<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    limit: i64,
) -> Result<u64> {
    if limit < 0 {
        return Err(crate::error::FusilladeError::ValidationError(
            "template route cleanup limit must not be negative".to_string(),
        ));
    }
    if limit == 0 {
        return Ok(0);
    }
    let mut transaction = manager.begin_write().await.map_err(|_| failed())?;
    let deleted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH candidates AS MATERIALIZED (
            SELECT route.template_id
            FROM request_template_routes route
            JOIN request_template_buckets bucket
              ON bucket.week_start = route.week_start
            WHERE bucket.state = 'retired'
            ORDER BY route.week_start, route.template_id
            FOR UPDATE OF route SKIP LOCKED
            LIMIT $1
        ), removed AS (
            DELETE FROM request_template_routes route
            USING candidates
            WHERE route.template_id = candidates.template_id
            RETURNING 1
        )
        SELECT COUNT(*)::bigint FROM removed
        "#,
    )
    .bind(limit)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| failed())?;
    transaction.commit().await.map_err(|_| failed())?;
    u64::try_from(deleted).map_err(|_| failed())
}
