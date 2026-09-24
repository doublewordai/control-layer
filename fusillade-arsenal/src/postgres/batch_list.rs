//! Customer batch pages: bound each sort group before attaching live counts.

use chrono::{DateTime, Utc};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::batch::ListBatchesFilter;
use crate::error::{FusilladeError, Result};

// Match idx_batches_active exactly. Cancelling is terminal, even
// before cancelled_at is stamped. Keep both arms exact complements.
const ACTIVE: &str = "b.completed_at IS NULL AND b.failed_at IS NULL \
    AND b.cancelled_at IS NULL AND b.cancelling_at IS NULL";

/// Statement budget for one page. Measured shapes finish in milliseconds; this
/// only trips on filters that walk an owner's whole history (search, COR-652).
pub(super) const PAGE_BUDGET: &str = "15s";

#[derive(Clone, Copy)]
pub(super) struct BatchCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
    pub priority: i32,
}

pub(super) fn push_query<'a>(
    query_builder: &mut QueryBuilder<'a, Postgres>,
    filter: &'a ListBatchesFilter,
    cursor: Option<BatchCursor>,
) -> Result<()> {
    let active_first = filter.active_first;
    let include_active = active_first && cursor.is_none_or(|c| c.priority == 0);
    let limit = filter.limit.unwrap_or(100);
    query_builder.push("WITH ");
    if active_first {
        // Materialize each sort group's own page: every predicate, the group
        // ordering and the page limit all live inside the arm. Splitting the
        // active and terminal groups into separately bounded CTEs keeps the
        // page's cost proportional to the page, not to the live backlog.
        //
        // The previous shape fenced the whole active set first
        // (`active_batches`) and applied the arm's ordering, filters and
        // limit afterwards. That cost O(active batches) per request for
        // platform-wide (unscoped) views — materializing and sorting every
        // live batch's full row — and O(active batches) of index I/O even for
        // owner-scoped views, since the owner predicate filtered the fenced
        // set only after it was read. When the live backlog grows faster than
        // batches complete (a customer fan-out creating thousands of batches
        // in an afternoon), every list page degraded with it and tripped the
        // 15s page budget, surfacing as 5xx on /ai/v1/batches.
        //
        // Ordering inside the arm lets the active-first expression index
        // (unscoped) and the owner active partial index (scoped) walk
        // straight to the page and stop at the limit.
        if include_active {
            query_builder.push("active_page AS MATERIALIZED ");
            push_arm(
                query_builder,
                filter,
                "batches",
                false,
                cursor.filter(|c| c.priority == 0),
                limit,
            )?;
            query_builder.push(", ");
        }
        query_builder.push("terminal_page AS MATERIALIZED ");
        push_arm(
            query_builder,
            filter,
            "batches",
            true,
            cursor.filter(|c| c.priority == 1),
            limit,
        )?;
        query_builder.push(", filtered AS MATERIALIZED (");
        if include_active {
            query_builder.push("SELECT * FROM active_page UNION ALL ");
        }
        query_builder
            .push("SELECT * FROM terminal_page ORDER BY priority, created_at DESC, id DESC LIMIT ");
        query_builder.push_bind(limit);
    } else {
        // Chronological listings walk one index-ordered, cursor-bounded arm;
        // they never fence a group.
        query_builder.push("filtered AS MATERIALIZED (");
        push_arm(query_builder, filter, "batches", false, cursor, limit)?;
    }
    query_builder.push(") ");
    let phase2_order = if active_first {
        "ORDER BY b.priority ASC, b.created_at DESC, b.id DESC"
    } else {
        "ORDER BY b.created_at DESC, b.id DESC"
    };
    query_builder.push(
            r#"
            SELECT
                b.id, b.file_id, b.endpoint, b.service_tier, b.completion_window, b.metadata,
                b.output_file_id, b.error_file_id, b.created_by, b.created_at,
                b.expires_at, b.cancelling_at, b.errors,
                b.total_requests,
                b.requests_started_at,
                b.finalizing_at,
                b.completed_at,
                b.failed_at,
                b.cancelled_at,
                b.deleted_at,
                b.notification_sent_at,
                b.api_key_id,
                CASE WHEN b.counts_frozen_at IS NOT NULL THEN 0
                     ELSE COALESCE(counts.pending, 0) END::BIGINT as pending_requests,
                CASE WHEN b.counts_frozen_at IS NOT NULL THEN 0
                     ELSE COALESCE(counts.in_progress, 0) END::BIGINT as in_progress_requests,
                -- Frozen batches serve the persisted counters. For live
                -- batches, `total_requests` is conserved once population
                -- finishes (rows inserted at batch creation, never deleted),
                -- so completed is derivable. Skipping the 'completed' scan
                -- in the LATERAL saves the bulk of the work on terminal
                -- batches, which can have millions of completed rows.
                --
                -- The `requests_started_at IS NULL` guard handles the
                -- validating window: `total_requests` is set at batch
                -- creation but request rows haven't been inserted yet,
                -- so all the LATERAL counts are zero. Without the guard,
                -- `total - 0 - 0 - 0 - 0` would report the missing rows
                -- as completed instead of 0.
                CASE WHEN b.counts_frozen_at IS NOT NULL THEN b.completed_requests
                     WHEN b.requests_started_at IS NULL THEN 0
                     ELSE GREATEST(b.total_requests
                         - COALESCE(counts.pending, 0)
                         - COALESCE(counts.in_progress, 0)
                         - COALESCE(counts.failed, 0)
                         - COALESCE(counts.canceled, 0), 0)
                END::BIGINT as completed_requests,
                CASE WHEN b.counts_frozen_at IS NOT NULL THEN b.failed_requests
                     ELSE COALESCE(counts.failed, 0) END::BIGINT as failed_requests,
                CASE WHEN b.counts_frozen_at IS NOT NULL THEN b.canceled_requests
                     ELSE COALESCE(counts.canceled, 0) END::BIGINT as canceled_requests
            FROM filtered b
            LEFT JOIN LATERAL (
                SELECT
                    COUNT(*) FILTER (WHERE state = 'pending' AND b.cancelling_at IS NULL) as pending,
                    COUNT(*) FILTER (WHERE state IN ('claimed', 'processing') AND b.cancelling_at IS NULL) as in_progress,
                    COUNT(*) FILTER (WHERE state = 'failed') as failed,
                    COUNT(*) FILTER (WHERE state = 'canceled' OR (state IN ('pending', 'claimed', 'processing') AND b.cancelling_at IS NOT NULL)) as canceled
                FROM requests
                WHERE batch_id = b.id
                  -- Frozen batches serve persisted counters; one-time filter
                  -- skips the requests scan entirely.
                  AND b.counts_frozen_at IS NULL
                  -- Skip the 'completed' slice — it's typically the bulk
                  -- of the index for terminal batches and we derive
                  -- the count arithmetically above. Enumerated states
                  -- let `idx_requests_batch_state` do narrow range
                  -- probes instead of a full scan.
                  AND state = ANY(ARRAY['pending', 'claimed', 'processing', 'failed', 'canceled'])
            ) counts ON TRUE
            "#,
        );
    query_builder.push(phase2_order);

    Ok(())
}

/// One ordering group, newest first, capped at `limit`. The terminal arm
/// selects the complement of `ACTIVE` and labels its rows priority 1; the
/// active arm and the plain chronological listing both carry priority 0.
fn push_arm<'a>(
    query_builder: &mut QueryBuilder<'a, Postgres>,
    filter: &'a ListBatchesFilter,
    table: &'static str,
    terminal: bool,
    cursor: Option<BatchCursor>,
    limit: i64,
) -> Result<()> {
    query_builder.push("(SELECT b.*, ");
    query_builder.push(if terminal { "1" } else { "0" });
    query_builder.push(" AS priority FROM ");
    query_builder.push(table);
    query_builder.push(" b");
    if filter.search.is_some() {
        query_builder.push(" LEFT JOIN files f ON b.file_id = f.id");
    }
    query_builder.push(" WHERE b.deleted_at IS NULL");
    if terminal {
        query_builder.push(" AND NOT (");
        query_builder.push(ACTIVE);
        query_builder.push(")");
    }
    // Presence changes SQL shape; values remain bound. Nullable OR predicates
    // cannot become owner/cursor index conditions in a generic prepared plan.
    if let Some(owner) = &filter.created_by {
        query_builder.push(" AND b.created_by = ").push_bind(owner);
    }
    if let Some(cursor) = cursor {
        query_builder.push(" AND (b.created_at, b.id) < (");
        query_builder.push_bind(cursor.created_at);
        query_builder.push(", ").push_bind(cursor.id).push(")");
    }
    if let Some(search) = &filter.search {
        let pattern = format!("%{}%", search.to_lowercase());
        query_builder
            .push(" AND (LOWER(b.metadata::text) LIKE ")
            .push_bind(pattern.clone());
        query_builder
            .push(" OR LOWER(f.name) LIKE ")
            .push_bind(pattern.clone());
        query_builder
            .push(" OR b.id::text LIKE ")
            .push_bind(pattern)
            .push(")");
    }
    if let Some(api_key_ids) = &filter.api_key_ids {
        query_builder.push(" AND b.api_key_id = ANY(");
        query_builder.push_bind(api_key_ids.as_slice());
        query_builder.push(")");
    }

    if let Some(created_after) = &filter.created_after {
        query_builder.push(" AND b.created_at >= ");
        query_builder.push_bind(*created_after);
    }

    if let Some(created_before) = &filter.created_before {
        query_builder.push(" AND b.created_at <= ");
        query_builder.push_bind(*created_before);
    }

    // Status filtering: map status names to DB column conditions.
    // All filters use persisted batch columns only — no dependency on request counts.
    // Derived sub-statuses (validating, finalizing) are resolved by the frontend
    // from the count data attached in the second phase of this query.
    if let Some(status) = &filter.status {
        match status.as_str() {
            "in_progress" => {
                // All non-terminal batches: covers validating, in_progress, and finalizing
                query_builder.push(" AND b.completed_at IS NULL AND b.failed_at IS NULL AND b.cancelled_at IS NULL AND b.cancelling_at IS NULL");
            }
            "completed" => {
                query_builder.push(" AND b.completed_at IS NOT NULL");
            }
            "failed" => {
                query_builder.push(" AND b.failed_at IS NOT NULL AND b.completed_at IS NULL");
            }
            "cancelled" => {
                // Includes both cancelling and fully cancelled batches
                query_builder
                    .push(" AND (b.cancelled_at IS NOT NULL OR b.cancelling_at IS NOT NULL)");
            }
            "expired" => {
                // Matches batches with SLA issues: either still in-progress past deadline,
                // or terminal batches that finished after their deadline.
                query_builder.push(
                        " AND b.expires_at IS NOT NULL AND (\
                            (b.expires_at < NOW() AND b.completed_at IS NULL AND b.failed_at IS NULL AND b.cancelled_at IS NULL AND b.cancelling_at IS NULL) \
                            OR (b.completed_at IS NOT NULL AND b.completed_at > b.expires_at) \
                            OR (b.failed_at IS NOT NULL AND b.failed_at > b.expires_at) \
                            OR (b.cancelled_at IS NOT NULL AND b.cancelled_at > b.expires_at)\
                        )",
                    );
            }
            unknown => {
                // Invalid client-supplied filter value - a bad request, not a server
                // fault. ValidationError so dwctl maps it to 400, not 500 (which pages).
                return Err(FusilladeError::ValidationError(format!(
                    "Unknown batch status filter: '{}'. Valid values: in_progress, completed, failed, cancelled, expired",
                    unknown
                )));
            }
        }
    }

    if let Some(tiers) = &filter.service_tiers
        && let Some(unknown) = tiers.iter().find(|tier| tier.as_str() != "background")
    {
        return Err(FusilladeError::ValidationError(format!(
            "Unknown batch service tier filter: '{unknown}'. Valid value: background"
        )));
    }

    // Completion windows and service tiers are two representations of the
    // same user-facing batch class filter. Combine them as a union when
    // both are present so callers can request regular and background
    // batches together.
    match (&filter.completion_windows, &filter.service_tiers) {
        (Some(windows), Some(tiers)) => {
            query_builder.push(" AND (b.completion_window = ANY(");
            query_builder.push_bind(windows.as_slice());
            query_builder.push(") OR b.service_tier = ANY(");
            query_builder.push_bind(tiers.as_slice());
            query_builder.push("))");
        }
        (Some(windows), None) => {
            query_builder.push(" AND b.completion_window = ANY(");
            query_builder.push_bind(windows.as_slice());
            query_builder.push(")");
        }
        (None, Some(tiers)) => {
            query_builder.push(" AND b.service_tier = ANY(");
            query_builder.push_bind(tiers.as_slice());
            query_builder.push(")");
        }
        (None, None) => {}
    }

    query_builder.push(" ORDER BY b.created_at DESC, b.id DESC LIMIT ");
    query_builder.push_bind(limit).push(")");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PostgresRequestManager, Storage, TestDbPools};
    use serde_json::Value;
    use sqlx::{PgPool, Row};

    fn batch_rows_visited(plan: &Value) -> f64 {
        match plan {
            Value::Array(values) => values.iter().map(batch_rows_visited).sum(),
            Value::Object(fields) => {
                let visited =
                    if fields.get("Relation Name").and_then(Value::as_str) == Some("batches") {
                        [
                            "Actual Rows",
                            "Rows Removed by Filter",
                            "Rows Removed by Index Recheck",
                        ]
                        .iter()
                        .map(|key| fields.get(*key).and_then(Value::as_f64).unwrap_or(0.0))
                        .sum::<f64>()
                            * fields
                                .get("Actual Loops")
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0)
                    } else {
                        0.0
                    };
                visited + fields.values().map(batch_rows_visited).sum::<f64>()
            }
            _ => 0.0,
        }
    }

    #[sqlx::test]
    async fn owner_pages_bound_history_under_generic_plans(pool: PgPool) {
        // Old owner history, newer unrelated history, and an active batch older
        // than either: scanning from the present or sorting the owner's history
        // both fail the work bound. Freeze counts to isolate page selection.
        sqlx::query(
            r#"
            INSERT INTO batches (id, endpoint, completion_window, created_by,
                                 created_at, completed_at, counts_frozen_at, expires_at)
            SELECT md5(i::text)::uuid, '/v1/chat/completions', '24h',
                   CASE WHEN i <= 2000 THEN 'owner' ELSE 'other-' || (i % 100)::text END,
                   '2026-01-01'::timestamptz + i * interval '1 second',
                   CASE WHEN i = 1 THEN NULL ELSE '2026-02-01'::timestamptz END,
                   '2026-02-01'::timestamptz, '2026-02-02'::timestamptz
            FROM generate_series(1, 4000) i
        "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let indexes: Vec<String> =
            sqlx::query_scalar("SELECT indexname FROM pg_indexes WHERE tablename = 'batches'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            indexes
                .iter()
                .any(|i| i == "idx_batches_owner_created_at_id"),
            "missing new index: {indexes:?}"
        );
        sqlx::query("ANALYZE batches").execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL plan_cache_mode = force_generic_plan")
            .execute(&mut *tx)
            .await
            .unwrap();
        let cursor_row =
            sqlx::query("SELECT id, created_at FROM batches WHERE id = md5('1000')::uuid")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        let terminal_cursor = BatchCursor {
            id: cursor_row.get("id"),
            created_at: cursor_row.get("created_at"),
            priority: 1,
        };
        let active_row =
            sqlx::query("SELECT id, created_at FROM batches WHERE id = md5('1')::uuid")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        let active_cursor = BatchCursor {
            id: active_row.get("id"),
            created_at: active_row.get("created_at"),
            priority: 0,
        };
        for active_first in [false, true] {
            for cursor in [None, Some(terminal_cursor), Some(active_cursor)] {
                // Chronological listing after the oldest batch is empty; the
                // active-first equivalent must still admit newer terminal rows.
                let filter = ListBatchesFilter {
                    created_by: Some("owner".into()),
                    active_first,
                    limit: Some(10),
                    ..Default::default()
                };
                let mut query = QueryBuilder::new("");
                push_query(&mut query, &filter, cursor).unwrap();
                sqlx::raw_sql(&format!("PREPARE owner_page AS {}", query.sql()))
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                // EXPLAIN with bound arguments can plan the inner statement as
                // a custom plan. Explicit PREPARE/EXECUTE exercises the cached
                // generic plan used after repeated SQLx executions.
                // Shape-specific by design: this filter binds only the owner
                // text, the bigint limit, and the cursor pair. A new parameter
                // type means the query shape under test changed.
                let types: Vec<String> = sqlx::query_scalar("SELECT unnest(parameter_types)::text FROM pg_prepared_statements WHERE name = 'owner_page'")
                    .fetch_all(&mut *tx).await.unwrap();
                let args: Vec<String> = types
                    .iter()
                    .map(|kind| match kind.as_str() {
                        "text" => "'owner'".into(),
                        "bigint" => "10".into(),
                        "timestamp with time zone" => format!("'{}'", cursor.unwrap().created_at),
                        "uuid" => format!("'{}'", cursor.unwrap().id),
                        _ => panic!("unexpected batch-page parameter type: {kind}"),
                    })
                    .collect();
                let plan: Value = sqlx::query_scalar(&format!(
                    "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) EXECUTE owner_page({})",
                    args.join(",")
                ))
                .fetch_one(&mut *tx)
                .await
                .unwrap();
                sqlx::raw_sql("DEALLOCATE owner_page")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                let expected_rows = if !active_first && cursor.is_some_and(|c| c.priority == 0) {
                    0.0
                } else {
                    10.0
                };
                assert_eq!(plan[0]["Plan"]["Actual Rows"].as_f64(), Some(expected_rows));
                let visited = batch_rows_visited(&plan);
                assert!(
                    visited < 100.0,
                    "active_first={active_first}, cursor={:?}: visited {visited} batch rows: {plan}",
                    cursor.map(|c| c.priority)
                );
            }
        }
    }

    #[sqlx::test]
    async fn owner_page_materializes_only_its_own_backlog(pool: PgPool) {
        // Both owners have more active rows than fit on a page. Filtering only
        // after the CTE would materialize 2,020 rows instead of this owner's
        // page; the arm carries its predicates and limit inside, so the CTE
        // holds exactly the page.
        sqlx::query(
            r#"
            INSERT INTO batches (id, endpoint, completion_window, created_by,
                                 created_at, counts_frozen_at, expires_at)
            SELECT md5(i::text)::uuid, '/v1/chat/completions', '24h',
                   CASE WHEN i <= 20 THEN 'owner' ELSE 'other' END,
                   '2026-01-01'::timestamptz + i * interval '1 second',
                   NOW(), NOW() + interval '1 day'
            FROM generate_series(1, 2020) i
        "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE batches").execute(&pool).await.unwrap();
        let filter = ListBatchesFilter {
            created_by: Some("owner".into()),
            active_first: true,
            limit: Some(10),
            ..Default::default()
        };
        let mut query = QueryBuilder::new("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ");
        push_query(&mut query, &filter, None).unwrap();
        let plan: Value = query.build_query_scalar().fetch_one(&pool).await.unwrap();
        let active = plan[0]["Plan"]["Plans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["Subplan Name"] == "CTE active_page")
            .expect("materialized active page");
        // EXPLAIN can encode row counts as integers or decimal numbers.
        assert_eq!(active["Actual Rows"].as_f64(), Some(10.0), "{plan}");
        assert_eq!(plan[0]["Plan"]["Actual Rows"].as_f64(), Some(10.0));
        // And the page is the owner's newest active batches, not another
        // owner's — the owner predicate must stay inside the arm.
        let expected: Vec<Uuid> = sqlx::query_scalar(
            r#"
            SELECT id FROM batches
            WHERE created_by = 'owner' AND deleted_at IS NULL
              AND completed_at IS NULL AND failed_at IS NULL
              AND cancelled_at IS NULL AND cancelling_at IS NULL
            ORDER BY created_at DESC, id DESC LIMIT 10
        "#,
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut query = QueryBuilder::new("");
        push_query(&mut query, &filter, None).unwrap();
        let actual: Vec<Uuid> = query.build_query_scalar().fetch_all(&pool).await.unwrap();
        assert_eq!(actual, expected);
    }

    #[sqlx::test]
    async fn unscoped_active_pages_bound_the_live_backlog(pool: PgPool) {
        // Platform managers list every owner's batches. A growing live backlog
        // (customer fan-out) must not grow the page's cost with it: the page
        // materializes its own top rows and returns the globally newest
        // active batches. Counts are frozen to isolate page selection. Newest
        // rows are terminal so both sort groups are reachable from the front
        // of the chronological index.
        sqlx::query(
            r#"
            INSERT INTO batches (id, endpoint, completion_window, created_by,
                                 created_at, counts_frozen_at, completed_at, expires_at)
            SELECT md5(i::text)::uuid, '/v1/chat/completions', '24h',
                   'owner-' || (i % 50)::text,
                   '2026-01-01'::timestamptz + i * interval '1 second',
                   CASE WHEN i <= 3000 THEN NULL ELSE NOW() END,
                   CASE WHEN i <= 3000 THEN NULL ELSE NOW() END,
                   NOW() + interval '1 day'
            FROM generate_series(1, 3020) i
        "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE batches").execute(&pool).await.unwrap();
        let filter = ListBatchesFilter {
            active_first: true,
            limit: Some(10),
            ..Default::default()
        };
        let mut query = QueryBuilder::new("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ");
        push_query(&mut query, &filter, None).unwrap();
        let plan: Value = query.build_query_scalar().fetch_one(&pool).await.unwrap();
        // The active page holds only the page's worth of live rows, not the
        // 3,000-row backlog the old fence materialized.
        let active = plan[0]["Plan"]["Plans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["Subplan Name"] == "CTE active_page")
            .expect("materialized active page");
        assert_eq!(active["Actual Rows"].as_f64(), Some(10.0), "{plan}");
        assert_eq!(plan[0]["Plan"]["Actual Rows"].as_f64(), Some(10.0));
        let visited = batch_rows_visited(&plan);
        assert!(visited < 100.0, "visited {visited} batch rows: {plan}");
        // The page is the globally newest active batches, unscoped.
        let expected: Vec<Uuid> = sqlx::query_scalar(
            r#"
            SELECT id FROM batches
            WHERE deleted_at IS NULL AND completed_at IS NULL AND failed_at IS NULL
              AND cancelled_at IS NULL AND cancelling_at IS NULL
            ORDER BY created_at DESC, id DESC LIMIT 10
        "#,
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut query = QueryBuilder::new("");
        push_query(&mut query, &filter, None).unwrap();
        let actual: Vec<Uuid> = query.build_query_scalar().fetch_all(&pool).await.unwrap();
        assert_eq!(actual, expected);
    }

    #[sqlx::test]
    async fn owner_and_admin_pages_preserve_ties_and_filters(pool: PgPool) {
        sqlx::query(r#"
            INSERT INTO batches (id, endpoint, created_by, created_at,
                completed_at, failed_at, cancelled_at, cancelling_at, deleted_at,
                service_tier, completion_window, expires_at, metadata, api_key_id,
                counts_frozen_at, total_requests, completed_requests, failed_requests, canceled_requests)
            SELECT md5(i::text)::uuid, '/v1/chat/completions',
                CASE WHEN i <= 20 THEN 'owner' ELSE 'other' END,
                '2026-01-01'::timestamptz + (i / 3) * interval '1 second',
                CASE WHEN i % 5 = 1 THEN NOW() END,
                CASE WHEN i % 5 = 2 THEN NOW() END,
                CASE WHEN i % 5 = 3 THEN NOW() END,
                CASE WHEN i % 5 = 4 THEN NOW() END,
                CASE WHEN i = 10 THEN NOW() END,
                CASE WHEN i % 3 = 0 THEN 'background' END,
                CASE WHEN i % 3 != 0 THEN '24h' END,
                CASE WHEN i % 3 != 0 THEN NOW() + interval '1 day' END,
                jsonb_build_object('tag', CASE WHEN i % 2 = 0 THEN 'needle' ELSE 'hay' END),
                md5((i % 2)::text)::uuid,
                NOW(), 6, 3, 2, 1
            FROM generate_series(1, 34) i
        "#).execute(&pool).await.unwrap();
        let manager = PostgresRequestManager::new(
            TestDbPools::new(pool.clone()).await.unwrap(),
            Default::default(),
        );
        for owner in [Some("owner"), None] {
            for active_first in [false, true] {
                for filtered in [false, true] {
                    let mut filter = ListBatchesFilter {
                        created_by: owner.map(str::to_owned),
                        active_first,
                        limit: Some(2),
                        ..Default::default()
                    };
                    if filtered {
                        filter.search = Some("needle".into());
                        filter.completion_windows = Some(vec!["24h".into()]);
                        filter.service_tiers = Some(vec!["background".into()]);
                        filter.created_after = Some("2026-01-01T00:00:01Z".parse().unwrap());
                        filter.created_before = Some("2026-01-01T00:00:04Z".parse().unwrap());
                    }
                    // Deliberately unbounded reference sort, independent of the
                    // page builder and its split active/terminal arms.
                    let expected: Vec<Uuid> = sqlx::query_scalar(
                        r#"
                        SELECT id FROM batches
                        WHERE deleted_at IS NULL AND ($1::text IS NULL OR created_by = $1)
                          AND (NOT $2 OR (metadata->>'tag' = 'needle'
                            AND created_at BETWEEN '2026-01-01T00:00:01Z' AND '2026-01-01T00:00:04Z'
                            AND (completion_window = '24h' OR service_tier = 'background')))
                        ORDER BY CASE WHEN $3 AND completed_at IS NULL AND failed_at IS NULL
                            AND cancelled_at IS NULL AND cancelling_at IS NULL THEN 0 ELSE 1 END,
                            created_at DESC, id DESC
                    "#,
                    )
                    .bind(owner)
                    .bind(filtered)
                    .bind(active_first)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                    let mut actual = Vec::new();
                    loop {
                        let page = manager.list_batches(filter.clone()).await.unwrap();
                        if page.is_empty() {
                            break;
                        }
                        assert_eq!(page.len(), 2.min(expected.len() - actual.len()));
                        for batch in &page {
                            assert_eq!(batch.completed_requests, 3);
                            assert_eq!(batch.failed_requests, 2);
                            assert_eq!(batch.canceled_requests, 1);
                        }
                        filter.after = page.last().map(|b| b.id);
                        actual.extend(page.iter().map(|b| *b.id as Uuid));
                        assert!(actual.len() <= expected.len(), "pagination repeated rows");
                    }
                    assert_eq!(
                        actual, expected,
                        "owner={owner:?}, active_first={active_first}, filtered={filtered}"
                    );
                }
            }
        }
    }

    #[sqlx::test]
    async fn migration_rejects_a_same_name_index_with_wrong_order(pool: PgPool) {
        let validation =
            include_str!("../../migrations/20260910010001_validate_batch_owner_page_index.up.sql");
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql("DROP INDEX idx_batches_owner_created_at_id; CREATE INDEX idx_batches_owner_created_at_id ON batches (created_by, created_at, id) WHERE deleted_at IS NULL;")
            .execute(&mut *tx).await.unwrap();
        let error = sqlx::raw_sql(validation)
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("wrong definition"));
        tx.rollback().await.unwrap();
        sqlx::raw_sql(validation).execute(&pool).await.unwrap();
    }
}
