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
use crate::manager::{FileExpiryCursor, FileExpiryStep, RetainedResponseRetirementOutcome};
use crate::{PoolProvider, PostgresRequestManager};
use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use uuid::Uuid;

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

/// One step of [`expire_file_content`]: take the next `$2` due files after
/// the cursor (`$3`, `$4`), oldest upload first, and tombstone those that no
/// live batch still pins.
///
/// The slice bounds the files examined, not the files expired. Batches keep
/// their content for longer than their input files, so most due files stay
/// pinned for weeks, and a limit on expired rows alone would examine every
/// one of them in each statement.
///
/// The age bound and the cursor are written against the bare columns so
/// `idx_files_content_expiry_due` serves the slice as a range read.
const EXPIRE_FILE_CONTENT: &str = r#"
    WITH slice AS MATERIALIZED (
        SELECT id, created_at
        FROM files
        WHERE purpose = 'batch'
          AND deleted_at IS NULL
          AND created_at <= statement_timestamp() - make_interval(days => $1)
          AND (created_at, id) > (
              COALESCE($3::timestamptz, '-infinity'::timestamptz),
              COALESCE($4::uuid, '00000000-0000-0000-0000-000000000000'::uuid)
          )
        ORDER BY created_at, id
        LIMIT $2
    ),
    candidates AS MATERIALIZED (
        SELECT files.id
        FROM slice
        JOIN files ON files.id = slice.id
        WHERE files.deleted_at IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM batches b
              WHERE b.file_id = files.id
                AND b.deleted_at IS NULL
                AND b.retention_expired_at IS NULL
          )
        FOR UPDATE OF files SKIP LOCKED
    ),
    expired AS (
        UPDATE files
        SET deleted_at = statement_timestamp(),
            retention_expired_at = statement_timestamp(),
            status = 'expired'
        FROM candidates
        WHERE files.id = candidates.id
          AND files.deleted_at IS NULL
        RETURNING 1
    )
    SELECT (SELECT COUNT(*) FROM expired) AS expired,
           (SELECT COUNT(*) FROM slice) AS examined,
           (SELECT created_at FROM slice ORDER BY created_at DESC, id DESC LIMIT 1) AS last_created_at,
           (SELECT id FROM slice ORDER BY created_at DESC, id DESC LIMIT 1) AS last_id
    "#;

/// Tombstone input files whose content has aged out: the file row survives
/// with `retention_expired_at` set, downloads and new batch creation fail,
/// and its template window becomes droppable. A file is eligible only when
/// every batch that referenced it has already had its own content expired or
/// explicitly erased.
///
/// Examines at most `scan_limit` due files after `after`. A slice shorter
/// than `scan_limit` reached the retention horizon, so `next` is `None`.
pub(super) async fn expire_file_content<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    retention_days: i32,
    scan_limit: i64,
    after: Option<FileExpiryCursor>,
) -> Result<FileExpiryStep> {
    if retention_days < 1 || scan_limit < 1 {
        return Ok(FileExpiryStep {
            expired: 0,
            next: None,
        });
    }
    let mut transaction = manager
        .begin_maintenance_write()
        .await
        .map_err(|_| failed())?;
    let (expired, examined, last_created_at, last_id): (
        i64,
        i64,
        Option<DateTime<Utc>>,
        Option<Uuid>,
    ) = sqlx::query_as(EXPIRE_FILE_CONTENT)
        .bind(retention_days)
        .bind(scan_limit)
        .bind(after.map(|cursor| cursor.created_at))
        .bind(after.map(|cursor| cursor.id))
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| failed())?;
    transaction.commit().await.map_err(|_| failed())?;
    let next = match (last_created_at, last_id) {
        (Some(created_at), Some(id)) if examined >= scan_limit => {
            Some(FileExpiryCursor { created_at, id })
        }
        _ => None,
    };
    Ok(FileExpiryStep {
        expired: u64::try_from(expired).map_err(|_| failed())?,
        next,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestDbPools;

    /// A long history of recent uploads must not be scanned to find the few
    /// files past the retention horizon.
    #[sqlx::test]
    async fn expiry_reads_only_files_past_the_horizon(pool: sqlx::PgPool) {
        sqlx::query(
            r#"
            INSERT INTO files (name, purpose, created_at)
            SELECT 'upload-' || i, 'batch',
                   CASE WHEN i <= 5 THEN now() - interval '60 days' ELSE now() END
            FROM generate_series(1, 20005) i
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE files").execute(&pool).await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL plan_cache_mode = force_generic_plan")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::raw_sql(&format!(
            "PREPARE expire_file_content(int, bigint, timestamptz, uuid) AS {EXPIRE_FILE_CONTENT}"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        let plan: serde_json::Value = sqlx::query_scalar(
            "EXPLAIN (FORMAT JSON) EXECUTE expire_file_content(30, 100, NULL, NULL)",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        // The index must bound the scan at the horizon and start it at the
        // cursor, not merely supply the ordering: both are index conditions.
        fn find_node<'a>(
            node: &'a serde_json::Value,
            index: &str,
        ) -> Option<&'a serde_json::Value> {
            if node["Index Name"] == index {
                return Some(node);
            }
            node["Plans"]
                .as_array()?
                .iter()
                .find_map(|child| find_node(child, index))
        }
        let node =
            find_node(&plan[0]["Plan"], "idx_files_content_expiry_due").unwrap_or_else(|| {
                panic!("expiry candidates must come from idx_files_content_expiry_due: {plan}")
            });
        assert!(
            matches!(
                node["Node Type"].as_str(),
                Some("Index Scan" | "Index Only Scan")
            ),
            "{plan}"
        );
        let cond = node["Index Cond"].as_str().unwrap_or_default();
        assert!(
            cond.contains("created_at <="),
            "age bound must be an index condition: {plan}"
        );
        assert!(
            cond.contains("ROW(created_at, id) >"),
            "cursor must be an index condition: {plan}"
        );

        let manager = PostgresRequestManager::new(
            TestDbPools::new(pool.clone()).await.unwrap(),
            Default::default(),
        );
        let step = expire_file_content(&manager, 30, 100, None).await.unwrap();
        assert_eq!(
            step,
            FileExpiryStep {
                expired: 5,
                next: None
            }
        );
        let step = expire_file_content(&manager, 30, 100, None).await.unwrap();
        assert_eq!(step.expired, 0);
    }

    /// Due files that live batches still pin must not make a step examine
    /// more than its slice: the walk covers them in bounded steps and expires
    /// only the unpinned files among them.
    #[sqlx::test]
    async fn expiry_walks_pinned_files_in_bounded_steps(pool: sqlx::PgPool) {
        // Seven due files, oldest first; every other one is pinned by a batch
        // whose content has not expired.
        sqlx::query(
            r#"
            INSERT INTO files (name, purpose, created_at)
            SELECT 'upload-' || i, 'batch', now() - interval '60 days' + i * interval '1 minute'
            FROM generate_series(1, 7) i
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let files: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM files ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .unwrap();
        let pinned: Vec<Uuid> = files.iter().copied().step_by(2).collect();
        sqlx::query(
            "INSERT INTO batches (file_id, endpoint, completion_window, expires_at) \
             SELECT id, '/v1/chat/completions', '24h', now() FROM UNNEST($1::uuid[]) AS id",
        )
        .bind(&pinned)
        .execute(&pool)
        .await
        .unwrap();

        let manager = PostgresRequestManager::new(
            TestDbPools::new(pool.clone()).await.unwrap(),
            Default::default(),
        );
        let mut steps = Vec::new();
        let mut after = None;
        loop {
            let step = expire_file_content(&manager, 30, 3, after).await.unwrap();
            steps.push(step.expired);
            match step.next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        // Slices of 3, 3 and 1 files; the last one is short, ending the walk.
        assert_eq!(steps, vec![1, 2, 0]);

        let expired: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM files WHERE id = ANY($1) AND retention_expired_at IS NOT NULL",
        )
        .bind(&files)
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut unpinned: Vec<Uuid> = files.iter().copied().skip(1).step_by(2).collect();
        let mut expired = expired;
        unpinned.sort();
        expired.sort();
        assert_eq!(expired, unpinned);
    }
}
