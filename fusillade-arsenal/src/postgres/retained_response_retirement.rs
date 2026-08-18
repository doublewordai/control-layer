//! Daily retained-response partition retirement and its bounded
//! route/fence cleanup phases.
//!
//! The crash-safe state machine lives in [`super::partition_retirement`];
//! this module contributes the daily family description (names, one-day
//! bounds, eligibility by PostgreSQL's UTC date) plus the content-free
//! route-to-fence transfer that follows physical retirement.

use crate::error::{FusilladeError, Result};
use crate::manager::{RetainedResponseMaintenanceError, RetainedResponseRetirementOutcome};
use crate::{PoolProvider, PostgresRequestManager};
use chrono::NaiveDate;

use super::partition_retirement::{FamilySpec, failed, retire};

fn any_lower(_lower: NaiveDate) -> bool {
    true
}

fn daily_child(delete_on: NaiveDate) -> String {
    format!("retained_response_objects_d{}", delete_on.format("%Y%m%d"))
}

const DAILY_RESPONSE_FAMILY: FamilySpec = FamilySpec {
    parent: "retained_response_objects",
    bucket_table: "retained_response_buckets",
    bucket_key: "delete_on",
    bounds_days: 1,
    valid_lower: any_lower,
    canonical_child: daily_child,
    orphan_fence_sql: r#"
        SELECT EXISTS (
            SELECT 1
            FROM retained_response_buckets bucket
            WHERE (
                bucket.state = 'retiring'
                AND NOT EXISTS (
                    SELECT 1
                    FROM retention_partition_retirements journal
                    WHERE journal.parent_table = 'retained_response_objects'
                      AND journal.partition_schema = bucket.partition_schema
                      AND journal.partition_table = bucket.partition_table
                      AND journal.partition_oid = bucket.partition_oid
                      AND journal.lower_bound = bucket.delete_on
                      AND journal.upper_bound = bucket.delete_on + 1
                      AND journal.completed_at IS NULL
                )
            ) OR (
                bucket.state = 'retired'
                AND NOT EXISTS (
                    SELECT 1
                    FROM retention_partition_retirements journal
                    WHERE journal.parent_table = 'retained_response_objects'
                      AND journal.partition_schema = bucket.partition_schema
                      AND journal.partition_table = bucket.partition_table
                      AND journal.partition_oid = bucket.partition_oid
                      AND journal.lower_bound = bucket.delete_on
                      AND journal.upper_bound = bucket.delete_on + 1
                      AND journal.completed_at IS NOT NULL
                )
            )
        )
        "#,
    candidate_sql: r#"
        SELECT bucket.partition_schema,
               namespace.oid AS partition_schema_oid,
               parent.oid AS parent_oid,
               bucket.partition_table,
               bucket.partition_oid,
               bucket.delete_on AS lower_bound,
               bucket.delete_on + 1 AS upper_bound
        FROM retained_response_buckets bucket
        JOIN pg_namespace namespace
          ON namespace.nspname = bucket.partition_schema
        JOIN pg_class parent
          ON parent.relnamespace = namespace.oid
         AND parent.relname = 'retained_response_objects'
        WHERE bucket.state = 'active'
          AND bucket.delete_on
                <= (statement_timestamp() AT TIME ZONE 'UTC')::date
        ORDER BY bucket.delete_on
        LIMIT 1
        "#,
    candidate_binds_retention: false,
    completion_sql: None,
};

pub(super) async fn retire_expired_response_partition<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    select_new: bool,
) -> Result<RetainedResponseRetirementOutcome> {
    retire(manager, &DAILY_RESPONSE_FAMILY, select_new, None).await
}

pub(super) async fn cleanup_expired_response_fences<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    limit: i64,
) -> Result<u64> {
    if limit < 0 {
        return Err(FusilladeError::ValidationError(
            "retained response fence cleanup limit must not be negative".to_string(),
        ));
    }
    if limit == 0 {
        return Ok(0);
    }

    // Candidates are locked with SKIP LOCKED so a concurrent destructive
    // lifecycle action holding a fence row never blocks cleanup, and the
    // outer expiry predicate re-verifies each locked row version so a fence
    // renewed or upgraded after candidate selection survives.
    let mut transaction = manager.begin_write().await.map_err(|_| failed())?;
    let deleted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH candidates AS MATERIALIZED (
            SELECT fence.object_id
            FROM retained_response_resurrection_fences fence
            WHERE fence.expires_at <= statement_timestamp()
            ORDER BY fence.expires_at, fence.object_id
            FOR UPDATE SKIP LOCKED
            LIMIT $1
        ), removed AS (
            DELETE FROM retained_response_resurrection_fences fence
            USING candidates
            WHERE fence.object_id = candidates.object_id
              AND fence.expires_at <= statement_timestamp()
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

pub(super) async fn cleanup_retained_response_routes<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    limit: i64,
) -> Result<u64> {
    if limit < 0 {
        return Err(FusilladeError::ValidationError(
            "retained response route cleanup limit must not be negative".to_string(),
        ));
    }
    if limit == 0 {
        return Ok(0);
    }
    let fence_seconds = manager.retained_response_fence_seconds().ok_or_else(|| {
        RetainedResponseMaintenanceError::FencePolicyMissing.into_fusillade_error()
    })?;
    let fence_seconds = i64::try_from(fence_seconds).map_err(|_| {
        FusilladeError::ValidationError(
            "retained response resurrection fence period is too large".to_string(),
        )
    })?;

    let mut transaction = manager.begin_write().await.map_err(|_| failed())?;
    let mut remaining = limit;
    let mut deleted = 0_u64;

    let request_deleted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH candidates AS MATERIALIZED (
            SELECT route.request_id AS object_id, journal.completed_at
            FROM retained_response_request_routes route
            JOIN retained_response_buckets bucket
              ON bucket.delete_on = route.delete_on
            JOIN retention_partition_retirements journal
              ON journal.parent_table = 'retained_response_objects'
             AND journal.partition_schema = bucket.partition_schema
             AND journal.partition_table = bucket.partition_table
             AND journal.partition_oid = bucket.partition_oid
             AND journal.lower_bound = bucket.delete_on
             AND journal.upper_bound = bucket.delete_on + 1
             AND journal.completed_at IS NOT NULL
             AND journal.completed_at = bucket.state_changed_at
            JOIN pg_namespace namespace
              ON namespace.nspname = bucket.partition_schema
             AND namespace.oid = journal.partition_schema_oid
            JOIN pg_class parent
              ON parent.relnamespace = namespace.oid
             AND parent.relname = 'retained_response_objects'
             AND parent.oid = journal.parent_oid
            WHERE bucket.state = 'retired'
              AND bucket.partition_schema = current_schema()
              AND bucket.partition_table = 'retained_response_objects_d'
                    || to_char(bucket.delete_on, 'YYYYMMDD')
            ORDER BY route.delete_on, route.request_id
            FOR UPDATE OF route SKIP LOCKED
            LIMIT $1
        ), fenced AS (
            INSERT INTO retained_response_resurrection_fences (
                object_id, reason, expires_at
            )
            SELECT object_id, 'retired',
                   completed_at + ($2::bigint * INTERVAL '1 second')
            FROM candidates
            ON CONFLICT (object_id) DO UPDATE
            SET reason = CASE
                    WHEN retained_response_resurrection_fences.reason = 'erased'
                        THEN 'erased'
                    ELSE 'retired'
                END,
                expires_at = GREATEST(
                    retained_response_resurrection_fences.expires_at,
                    EXCLUDED.expires_at
                )
            RETURNING object_id
        ), removed AS (
            DELETE FROM retained_response_request_routes route
            USING fenced
            WHERE route.request_id = fenced.object_id
            RETURNING 1
        )
        SELECT COUNT(*)::bigint FROM removed
        "#,
    )
    .bind(remaining)
    .bind(fence_seconds)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| failed())?;
    remaining -= request_deleted;
    deleted += u64::try_from(request_deleted).map_err(|_| failed())?;

    if remaining > 0 {
        let step_deleted = sqlx::query_scalar::<_, i64>(
            r#"
            WITH candidates AS MATERIALIZED (
                SELECT route.step_id AS object_id, journal.completed_at
                FROM retained_response_step_routes route
                JOIN retained_response_buckets bucket
                  ON bucket.delete_on = route.delete_on
                JOIN retention_partition_retirements journal
                  ON journal.parent_table = 'retained_response_objects'
                 AND journal.partition_schema = bucket.partition_schema
                 AND journal.partition_table = bucket.partition_table
                 AND journal.partition_oid = bucket.partition_oid
                 AND journal.lower_bound = bucket.delete_on
                 AND journal.upper_bound = bucket.delete_on + 1
                 AND journal.completed_at IS NOT NULL
                 AND journal.completed_at = bucket.state_changed_at
                JOIN pg_namespace namespace
                  ON namespace.nspname = bucket.partition_schema
                 AND namespace.oid = journal.partition_schema_oid
                JOIN pg_class parent
                  ON parent.relnamespace = namespace.oid
                 AND parent.relname = 'retained_response_objects'
                 AND parent.oid = journal.parent_oid
                WHERE bucket.state = 'retired'
                  AND bucket.partition_schema = current_schema()
                  AND bucket.partition_table = 'retained_response_objects_d'
                        || to_char(bucket.delete_on, 'YYYYMMDD')
                ORDER BY route.delete_on, route.step_id
                FOR UPDATE OF route SKIP LOCKED
                LIMIT $1
            ), fenced AS (
                INSERT INTO retained_response_resurrection_fences (
                    object_id, reason, expires_at
                )
                SELECT object_id, 'retired',
                       completed_at + ($2::bigint * INTERVAL '1 second')
                FROM candidates
                ON CONFLICT (object_id) DO UPDATE
                SET reason = CASE
                        WHEN retained_response_resurrection_fences.reason = 'erased'
                            THEN 'erased'
                        ELSE 'retired'
                    END,
                    expires_at = GREATEST(
                        retained_response_resurrection_fences.expires_at,
                        EXCLUDED.expires_at
                    )
                RETURNING object_id
            ), removed AS (
                DELETE FROM retained_response_step_routes route
                USING fenced
                WHERE route.step_id = fenced.object_id
                RETURNING 1
            )
            SELECT COUNT(*)::bigint FROM removed
            "#,
        )
        .bind(remaining)
        .bind(fence_seconds)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| failed())?;
        remaining -= step_deleted;
        deleted += u64::try_from(step_deleted).map_err(|_| failed())?;
    }

    if remaining > 0 {
        let group_deleted = sqlx::query_scalar::<_, i64>(
            r#"
            WITH candidates AS MATERIALIZED (
                SELECT route.group_id AS object_id, journal.completed_at
                FROM retained_response_group_routes route
                JOIN retained_response_buckets bucket
                  ON bucket.delete_on = route.delete_on
                JOIN retention_partition_retirements journal
                  ON journal.parent_table = 'retained_response_objects'
                 AND journal.partition_schema = bucket.partition_schema
                 AND journal.partition_table = bucket.partition_table
                 AND journal.partition_oid = bucket.partition_oid
                 AND journal.lower_bound = bucket.delete_on
                 AND journal.upper_bound = bucket.delete_on + 1
                 AND journal.completed_at IS NOT NULL
                 AND journal.completed_at = bucket.state_changed_at
                JOIN pg_namespace namespace
                  ON namespace.nspname = bucket.partition_schema
                 AND namespace.oid = journal.partition_schema_oid
                JOIN pg_class parent
                  ON parent.relnamespace = namespace.oid
                 AND parent.relname = 'retained_response_objects'
                 AND parent.oid = journal.parent_oid
                WHERE bucket.state = 'retired'
                  AND bucket.partition_schema = current_schema()
                  AND bucket.partition_table = 'retained_response_objects_d'
                        || to_char(bucket.delete_on, 'YYYYMMDD')
                  AND NOT EXISTS (
                      SELECT 1 FROM retained_response_request_routes request_route
                      WHERE request_route.group_id = route.group_id
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM retained_response_step_routes step_route
                      WHERE step_route.group_id = route.group_id
                  )
                ORDER BY route.delete_on, route.group_id
                FOR UPDATE OF route SKIP LOCKED
                LIMIT $1
            ), fenced AS (
                INSERT INTO retained_response_resurrection_fences (
                    object_id, reason, expires_at
                )
                SELECT object_id, 'retired',
                       completed_at + ($2::bigint * INTERVAL '1 second')
                FROM candidates
                ON CONFLICT (object_id) DO UPDATE
                SET reason = CASE
                        WHEN retained_response_resurrection_fences.reason = 'erased'
                            THEN 'erased'
                        ELSE 'retired'
                    END,
                    expires_at = GREATEST(
                        retained_response_resurrection_fences.expires_at,
                        EXCLUDED.expires_at
                    )
                RETURNING object_id
            ), removed AS (
                DELETE FROM retained_response_group_routes route
                USING fenced
                WHERE route.group_id = fenced.object_id
                  AND NOT EXISTS (
                      SELECT 1 FROM retained_response_request_routes request_route
                      WHERE request_route.group_id = route.group_id
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM retained_response_step_routes step_route
                      WHERE step_route.group_id = route.group_id
                  )
                RETURNING 1
            )
            SELECT COUNT(*)::bigint FROM removed
            "#,
        )
        .bind(remaining)
        .bind(fence_seconds)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| failed())?;
        deleted += u64::try_from(group_deleted).map_err(|_| failed())?;
    }

    transaction.commit().await.map_err(|_| failed())?;
    Ok(deleted)
}
