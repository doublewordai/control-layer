//! Row-level helpers for the generation-2 request template store.
//!
//! Every template lives in a weekly `request_templates_g2` partition and is
//! located through its `request_template_routes` row, so any point write or
//! delete has to touch both relations together: a template without a route is
//! unreachable, and a route without a template is a dangling location oracle.
//! These helpers keep that pairing in one place for the dedicated (batchless,
//! `file_id IS NULL`) templates that the request paths create and erase one at
//! a time; file ingestion has its own bulk `UNNEST` insert.

use anyhow::anyhow;
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::error::{FusilladeError, Result};

/// Lazily guarantee the partition for the UTC week at transaction start.
/// Inserts use the same transaction clock so crossing Monday is safe.
///
/// Existing active partitions use a shared bucket-row lock, allowing concurrent
/// writers while preventing retirement from fencing the bucket until commit.
/// Missing or inconsistent partitions go through the validating creation helper.
pub(crate) async fn ensure_current_week_partition(
    conn: &mut Transaction<'_, Postgres>,
) -> Result<()> {
    let active: Option<bool> = sqlx::query_scalar(
        r#"
        SELECT true
        FROM request_template_buckets bucket
        JOIN pg_class child ON child.oid = bucket.partition_oid
        JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
        JOIN pg_inherits inheritance ON inheritance.inhrelid = child.oid
        WHERE bucket.week_start = date_trunc('week', transaction_timestamp() AT TIME ZONE 'UTC')::date
          AND bucket.state = 'active'
          AND bucket.partition_schema = current_schema()
          AND namespace.nspname = bucket.partition_schema
          AND child.relname = bucket.partition_table
          AND bucket.partition_table = 'request_templates_g2_y'
              || to_char(bucket.week_start, 'IYYY') || 'w' || to_char(bucket.week_start, 'IW')
          AND inheritance.inhparent = 'request_templates_g2'::regclass
          AND NOT inheritance.inhdetachpending
          AND pg_get_expr(child.relpartbound, child.oid) = format(
              'FOR VALUES FROM (%L) TO (%L)', bucket.week_start, bucket.week_start + 7)
        FOR SHARE OF bucket
        "#,
    )
    .fetch_optional(&mut **conn)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to lock template bucket: {}", e)))?;
    if active == Some(true) {
        return Ok(());
    }
    sqlx::query(
        "SELECT ensure_request_template_partition( \
             date_trunc('week', transaction_timestamp() AT TIME ZONE 'UTC')::date, NULL)",
    )
    .execute(&mut **conn)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to ensure template partition: {}", e)))?;
    Ok(())
}

/// A dedicated (batchless) template to insert with its route.
pub(crate) struct DedicatedTemplate<'a> {
    pub(crate) id: Uuid,
    pub(crate) endpoint: &'a str,
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    pub(crate) body: &'a str,
    pub(crate) model: &'a str,
    pub(crate) api_key: &'a str,
    pub(crate) metadata: Option<&'a serde_json::Value>,
}

/// Insert one dedicated template into this week's partition and record its
/// route. The caller must have ensured the partition exists.
pub(crate) async fn insert_dedicated_template(
    conn: &mut Transaction<'_, Postgres>,
    template: DedicatedTemplate<'_>,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        WITH inserted AS (
            INSERT INTO request_templates_g2 (
                created_on, id, file_id, custom_id, endpoint, method, path,
                body, model, api_key, body_byte_size, metadata
            )
            VALUES (
                (transaction_timestamp() AT TIME ZONE 'UTC')::date, $1, NULL, NULL,
                $2, $3, $4, $5, $6, $7, $8, $9
            )
            RETURNING id, created_on
        )
        INSERT INTO request_template_routes (template_id, week_start)
        SELECT id, date_trunc('week', created_on)::date FROM inserted
        "#,
    )
    .bind(template.id)
    .bind(template.endpoint)
    .bind(template.method)
    .bind(template.path)
    .bind(template.body)
    .bind(template.model)
    .bind(template.api_key)
    .bind(template.body.len() as i64)
    .bind(template.metadata)
    .execute(&mut **conn)
    .await
    .map(|_| ())
}

/// Which dedicated templates a delete may take.
#[derive(Clone, Copy)]
pub(crate) enum DedicatedDeleteScope {
    /// Every dedicated template among the ids. Callers use this when they
    /// own the request that pointed at the template (single-request erasure,
    /// discarding a template whose request was never inserted).
    Any,
    /// Only dedicated templates that no `requests` row references any more,
    /// so a template shared with a still-live request survives.
    Unreferenced,
}

/// Delete dedicated (`file_id IS NULL`) templates by id, together with their
/// routes, and return how many were removed. File-backed templates are shared
/// across a batch and are never touched here; the orphan purge reaps those
/// after their file is soft-deleted.
pub(crate) async fn delete_dedicated_templates(
    conn: &mut PgConnection,
    template_ids: &[Uuid],
    scope: DedicatedDeleteScope,
) -> std::result::Result<u64, sqlx::Error> {
    if template_ids.is_empty() {
        return Ok(0);
    }
    let reference_guard = match scope {
        DedicatedDeleteScope::Any => "",
        DedicatedDeleteScope::Unreferenced => {
            "AND NOT EXISTS (SELECT 1 FROM requests WHERE requests.template_id = template.id)"
        }
    };
    let sql = format!(
        r#"
        WITH removed AS (
            DELETE FROM request_templates_g2 template
            USING request_template_routes route
            WHERE route.template_id = ANY($1)
              AND template.created_on >= route.week_start
              AND template.created_on < route.week_start + 7
              AND template.id = route.template_id
              AND template.file_id IS NULL
              {reference_guard}
            RETURNING template.id
        )
        DELETE FROM request_template_routes route
        USING removed
        WHERE route.template_id = removed.id
        "#
    );
    sqlx::query(&sql)
        .bind(template_ids)
        .execute(&mut *conn)
        .await
        .map(|result| result.rows_affected())
}

/// Count how many of the ids still resolve to a template row through the
/// route oracle.
pub(crate) async fn count_templates(
    conn: &mut PgConnection,
    template_ids: &[Uuid],
) -> std::result::Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM request_template_routes route
        JOIN request_templates_g2 template
          ON template.created_on >= route.week_start
         AND template.created_on < route.week_start + 7
         AND template.id = route.template_id
        WHERE route.template_id = ANY($1)
        "#,
    )
    .bind(template_ids)
    .fetch_one(&mut *conn)
    .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    #[sqlx::test]
    async fn existing_partition_rejects_a_retiring_bucket(pool: sqlx::PgPool) {
        let mut setup = pool.begin().await.unwrap();
        ensure_current_week_partition(&mut setup).await.unwrap();
        setup.commit().await.unwrap();
        sqlx::query("UPDATE request_template_buckets SET state = 'retiring'")
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(ensure_current_week_partition(&mut tx).await.is_err());
    }

    #[sqlx::test]
    async fn existing_partition_shares_writer_locks_and_blocks_retirement(pool: sqlx::PgPool) {
        let mut setup = pool.begin().await.unwrap();
        ensure_current_week_partition(&mut setup).await.unwrap();
        setup.commit().await.unwrap();
        let mut first = pool.begin().await.unwrap();
        ensure_current_week_partition(&mut first).await.unwrap();
        let mut second = pool.begin().await.unwrap();
        timeout(
            Duration::from_secs(2),
            ensure_current_week_partition(&mut second),
        )
        .await
        .expect("existing-partition writers must not serialize")
        .unwrap();
        let blocked =
            sqlx::query("SELECT week_start FROM request_template_buckets FOR UPDATE NOWAIT")
                .fetch_all(&pool)
                .await
                .unwrap_err();
        assert_eq!(
            blocked.as_database_error().unwrap().code().as_deref(),
            Some("55P03")
        );
        first.commit().await.unwrap();
        second.commit().await.unwrap();
        sqlx::query("UPDATE request_template_buckets SET state = 'retiring'")
            .execute(&pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn dedicated_insert_keeps_the_partition_ensured_before_a_week_boundary(
        pool: sqlx::PgPool,
    ) {
        let mut tx = pool.begin().await.unwrap();
        ensure_current_week_partition(&mut tx).await.unwrap();
        // Advance the statement clock into the next week without changing
        // the transaction clock or waiting for an actual Monday boundary.
        sqlx::raw_sql(
            "CREATE SCHEMA test_clock;
             CREATE FUNCTION test_clock.statement_timestamp() RETURNS timestamptz
             LANGUAGE sql AS $$ SELECT transaction_timestamp() + interval '7 days' $$;
             SET LOCAL search_path = test_clock, public, pg_catalog;",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        let id = Uuid::new_v4();
        insert_dedicated_template(
            &mut tx,
            DedicatedTemplate {
                id,
                endpoint: "https://example.invalid",
                method: "POST",
                path: "/test",
                body: "{}",
                model: "test-model",
                api_key: "test-key",
                metadata: None,
            },
        )
        .await
        .expect("insertion must use the week ensured within this transaction");
        let matches: bool = sqlx::query_scalar(
            "SELECT template.created_on = (transaction_timestamp() AT TIME ZONE 'UTC')::date
                    AND route.week_start = date_trunc('week', template.created_on)::date
             FROM request_templates_g2 template
             JOIN request_template_routes route ON route.template_id = template.id
             WHERE template.id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert!(matches);
    }
}
