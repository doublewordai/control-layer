//! Weekly generation-2 template retirement.
//!
//! Request input payloads are deleted by dropping whole weekly
//! `request_templates_g2` partitions. A week is eligible when it is past the
//! retention horizon, measured from the week itself, and no unfinished batch
//! still reads templates from it. File metadata rows are never touched: the
//! API reports an input file as expired from its own `expires_at`, which the
//! upload cap keeps no later than the week's drop.

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
    // The week must be past the horizon and no unfinished batch may still
    // read templates from it: a batch's templates are in the week its input
    // file was uploaded, so the guard joins the few dozen unfinished batches
    // (idx_batches_unfinished_created_at) to their files. A finished batch
    // holds nothing open; its results do not need the templates. A dedicated
    // (file-less) template row still fails the gate closed: nothing
    // unmanaged is dropped implicitly.
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
              FROM batches b
              JOIN files f ON f.id = b.file_id
              WHERE b.counts_frozen_at IS NULL
                AND b.deleted_at IS NULL
                AND f.created_at >= (bucket.week_start::timestamp AT TIME ZONE 'UTC')
                AND f.created_at < ((bucket.week_start + 7)::timestamp AT TIME ZONE 'UTC')
          )
          AND NOT EXISTS (
              SELECT 1
              FROM request_templates_g2 template
              WHERE template.created_on >= bucket.week_start
                AND template.created_on < bucket.week_start + 7
                AND template.file_id IS NULL
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
    let mut transaction = manager
        .begin_maintenance_write()
        .await
        .map_err(|_| failed())?;
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
