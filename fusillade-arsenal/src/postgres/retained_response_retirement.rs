//! Crash-safe daily partition retirement for retained response content.
//!
//! This is deliberately separate from weekly archive retirement. Daily
//! response partitions carry an immutable generation identity and a bucket
//! read fence that must be committed with the recovery journal.

use crate::error::{FusilladeError, Result};
use crate::manager::{RetainedResponseMaintenanceError, RetainedResponseRetirementOutcome};
use crate::{PoolProvider, PostgresRequestManager};
use chrono::NaiveDate;
use sqlx::postgres::types::Oid;
use sqlx::{Connection, Executor, PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

const PARENT: &str = "retained_response_objects";

#[derive(Debug, Clone)]
struct Identity {
    schema: String,
    schema_oid: Oid,
    parent_oid: Oid,
    child: String,
    child_oid: Oid,
    lower: NaiveDate,
    upper: NaiveDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attachment {
    Attached,
    DetachPending,
    Detached,
}

enum Claim {
    None,
    Contended,
    Claimed(Identity),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetachProgress {
    Detached,
    Retryable,
}

#[derive(Debug)]
struct RetirementContention;

impl std::fmt::Display for RetirementContention {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Retained response partition retirement was contended")
    }
}

impl std::error::Error for RetirementContention {}

fn error(kind: RetainedResponseMaintenanceError) -> FusilladeError {
    kind.into_fusillade_error()
}

fn failed() -> FusilladeError {
    error(RetainedResponseMaintenanceError::RetirementFailed)
}

fn mismatch() -> FusilladeError {
    error(RetainedResponseMaintenanceError::RetirementIdentityMismatch)
}

fn is_retryable(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if matches!(database.code().as_deref(), Some("55P03" | "57014"))
    )
}

fn retirement_database_error(error: sqlx::Error) -> FusilladeError {
    if is_retryable(&error) {
        FusilladeError::Other(anyhow::Error::new(RetirementContention))
    } else {
        failed()
    }
}

fn is_contention(error: &FusilladeError) -> bool {
    matches!(error, FusilladeError::Other(error) if error.is::<RetirementContention>())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn qualified(schema: &str, relation: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema),
        quote_identifier(relation)
    )
}

fn canonical_child(delete_on: NaiveDate) -> String {
    format!("retained_response_objects_d{}", delete_on.format("%Y%m%d"))
}

async fn configure_session(connection: &mut PgConnection) -> Result<()> {
    // These are server-side bounds, not client cancellation wrappers. Set
    // them on every acquired session before any retirement inspection or DDL.
    sqlx::query("SET SESSION lock_timeout = '5s'")
        .execute(&mut *connection)
        .await
        .map_err(retirement_database_error)?;
    sqlx::query("SET SESSION statement_timeout = '30s'")
        .execute(&mut *connection)
        .await
        .map_err(retirement_database_error)?;
    Ok(())
}

async fn configure_guard(transaction: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut **transaction)
        .await
        .map_err(retirement_database_error)?;
    sqlx::query("SET LOCAL statement_timeout = '30s'")
        .execute(&mut **transaction)
        .await
        .map_err(retirement_database_error)?;
    Ok(())
}

async fn lock_parent(transaction: &mut Transaction<'_, Postgres>) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(\
             hashtextextended('retained_response_objects.retirement:' || current_schema(), 0)\
         )",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(retirement_database_error)
}

async fn lock_partition(
    transaction: &mut Transaction<'_, Postgres>,
    delete_on: NaiveDate,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(\
             hashtextextended(\
                 'retained_response_objects.partition:' || current_schema() || ':'\
                     || to_char($1::date, 'YYYYMMDD'),\
                 0\
             )\
         )",
    )
    .bind(delete_on)
    .fetch_one(&mut **transaction)
    .await
    .map_err(retirement_database_error)
}

async fn inspect_identity<'e, E>(executor: E, identity: &Identity) -> Result<Attachment>
where
    E: Executor<'e, Database = Postgres>,
{
    if identity.upper != identity.lower + chrono::Duration::days(1)
        || identity.child != canonical_child(identity.lower)
    {
        return Err(mismatch());
    }

    let row = sqlx::query(
        r#"
        SELECT
            current_schema() AS manager_schema,
            current_schema_namespace.oid AS manager_schema_oid,
            parent_namespace.nspname AS parent_schema,
            parent_namespace.oid AS parent_schema_oid,
            parent.relname AS parent_name,
            parent.oid AS actual_parent_oid,
            partitioned.partdefid AS default_oid,
            child_namespace.nspname AS child_schema,
            child_namespace.oid AS child_schema_oid,
            child.relname AS child_name,
            child.oid AS actual_child_oid,
            pg_has_role(current_user, parent.relowner, 'USAGE') AS owns_parent,
            pg_has_role(current_user, child.relowner, 'USAGE') AS owns_child,
            named_child.oid AS named_child_oid,
            inheritance.inhparent AS attached_parent_oid,
            inheritance.inhdetachpending,
            CASE WHEN inheritance.inhrelid IS NULL THEN NULL
                 ELSE pg_get_expr(child.relpartbound, child.oid)
            END AS partition_bound
        FROM pg_class parent
        JOIN pg_namespace current_schema_namespace
          ON current_schema_namespace.nspname = current_schema()
        JOIN pg_namespace parent_namespace
          ON parent_namespace.oid = parent.relnamespace
        JOIN pg_partitioned_table partitioned
          ON partitioned.partrelid = parent.oid
        JOIN pg_class child ON child.oid = $2::oid
        JOIN pg_namespace child_namespace
          ON child_namespace.oid = child.relnamespace
        LEFT JOIN pg_class named_child
          ON named_child.relnamespace = parent_namespace.oid
         AND named_child.relname = $3
        LEFT JOIN pg_inherits inheritance
          ON inheritance.inhrelid = child.oid
        WHERE parent.oid = $1::oid
        "#,
    )
    .bind(identity.parent_oid)
    .bind(identity.child_oid)
    .bind(&identity.child)
    .fetch_optional(executor)
    .await
    .map_err(retirement_database_error)?
    .ok_or_else(mismatch)?;

    let exact = row
        .try_get::<String, _>("manager_schema")
        .is_ok_and(|value| value == identity.schema)
        && row
            .try_get::<Oid, _>("manager_schema_oid")
            .is_ok_and(|value| value == identity.schema_oid)
        && row
        .try_get::<String, _>("parent_schema")
        .is_ok_and(|value| value == identity.schema)
        && row
            .try_get::<Oid, _>("parent_schema_oid")
            .is_ok_and(|value| value == identity.schema_oid)
        && row
            .try_get::<String, _>("parent_name")
            .is_ok_and(|value| value == PARENT)
        && row
            .try_get::<Oid, _>("actual_parent_oid")
            .is_ok_and(|value| value == identity.parent_oid)
        && row
            .try_get::<String, _>("child_schema")
            .is_ok_and(|value| value == identity.schema)
        && row
            .try_get::<Oid, _>("child_schema_oid")
            .is_ok_and(|value| value == identity.schema_oid)
        && row
            .try_get::<String, _>("child_name")
            .is_ok_and(|value| value == identity.child)
        && row
            .try_get::<Oid, _>("actual_child_oid")
            .is_ok_and(|value| value == identity.child_oid)
        && row.try_get::<bool, _>("owns_parent").is_ok_and(|value| value)
        && row.try_get::<bool, _>("owns_child").is_ok_and(|value| value)
        && row
            .try_get::<Option<Oid>, _>("named_child_oid")
            .is_ok_and(|value| value == Some(identity.child_oid))
        // PostgreSQL does not support concurrent detach while any default
        // partition exists. Refuse the entire shape instead of falling back
        // to a stronger-lock DDL variant.
        && row
            .try_get::<Oid, _>("default_oid")
            .is_ok_and(|value| value == Oid(0));
    if !exact {
        return Err(mismatch());
    }

    let attached_parent = row
        .try_get::<Option<Oid>, _>("attached_parent_oid")
        .map_err(retirement_database_error)?;
    let detach_pending = row
        .try_get::<Option<bool>, _>("inhdetachpending")
        .map_err(retirement_database_error)?;
    match (attached_parent, detach_pending) {
        (None, None) => Ok(Attachment::Detached),
        (Some(parent_oid), Some(pending)) if parent_oid == identity.parent_oid => {
            let bound = row
                .try_get::<Option<String>, _>("partition_bound")
                .map_err(retirement_database_error)?;
            let expected = format!(
                "FOR VALUES FROM ('{}') TO ('{}')",
                identity.lower, identity.upper
            );
            if bound.as_deref() != Some(expected.as_str()) {
                return Err(mismatch());
            }
            Ok(if pending {
                Attachment::DetachPending
            } else {
                Attachment::Attached
            })
        }
        _ => Err(mismatch()),
    }
}

fn identity_from_row(row: &sqlx::postgres::PgRow) -> Result<Identity> {
    Ok(Identity {
        schema: row.try_get("partition_schema").map_err(|_| mismatch())?,
        schema_oid: row
            .try_get("partition_schema_oid")
            .map_err(|_| mismatch())?,
        parent_oid: row.try_get("parent_oid").map_err(|_| mismatch())?,
        child: row.try_get("partition_table").map_err(|_| mismatch())?,
        child_oid: row.try_get("partition_oid").map_err(|_| mismatch())?,
        lower: row.try_get("lower_bound").map_err(|_| mismatch())?,
        upper: row.try_get("upper_bound").map_err(|_| mismatch())?,
    })
}

async fn claim(connection: &mut PgConnection, owner: Uuid, select_new: bool) -> Result<Claim> {
    let mut transaction = connection
        .begin()
        .await
        .map_err(retirement_database_error)?;
    if !lock_parent(&mut transaction).await? {
        transaction
            .rollback()
            .await
            .map_err(retirement_database_error)?;
        return Ok(Claim::Contended);
    }

    let unfinished = sqlx::query(
        r#"
        SELECT partition_schema, partition_schema_oid,
               parent_oid, partition_table, partition_oid,
               lower_bound, upper_bound, lease_owner,
               COALESCE(lease_expires_at > statement_timestamp(), FALSE) AS lease_live
        FROM retention_partition_retirements
        WHERE parent_table = 'retained_response_objects'
          AND completed_at IS NULL
        ORDER BY requested_at, partition_table
        FOR UPDATE
        LIMIT 1
        "#,
    )
    .fetch_optional(&mut *transaction)
    .await
    .map_err(retirement_database_error)?;

    let (identity, recovering) = if let Some(row) = unfinished {
        let lease_owner: Option<Uuid> = row
            .try_get("lease_owner")
            .map_err(retirement_database_error)?;
        let lease_live: bool = row
            .try_get("lease_live")
            .map_err(retirement_database_error)?;
        if lease_live && lease_owner != Some(owner) {
            transaction
                .rollback()
                .await
                .map_err(retirement_database_error)?;
            return Ok(Claim::Contended);
        }
        (identity_from_row(&row)?, true)
    } else {
        let orphan_fence: bool = sqlx::query_scalar(
            r#"
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
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(retirement_database_error)?;
        if orphan_fence {
            return Err(mismatch());
        }
        if !select_new {
            transaction
                .commit()
                .await
                .map_err(retirement_database_error)?;
            return Ok(Claim::None);
        }
        let Some(row) = sqlx::query(
            r#"
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
        )
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retirement_database_error)?
        else {
            transaction
                .commit()
                .await
                .map_err(retirement_database_error)?;
            return Ok(Claim::None);
        };
        (identity_from_row(&row)?, false)
    };

    if !lock_partition(&mut transaction, identity.lower).await? {
        transaction
            .rollback()
            .await
            .map_err(retirement_database_error)?;
        return Ok(Claim::Contended);
    }
    let attachment = inspect_identity(&mut *transaction, &identity).await?;
    if !recovering && attachment != Attachment::Attached {
        return Err(mismatch());
    }

    if recovering {
        let bucket: Option<(String, String, String, Oid)> = sqlx::query_as(
            "SELECT state, partition_schema, partition_table, partition_oid \
             FROM retained_response_buckets WHERE delete_on = $1 FOR UPDATE",
        )
        .bind(identity.lower)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retirement_database_error)?;
        if bucket
            != Some((
                "retiring".to_string(),
                identity.schema.clone(),
                identity.child.clone(),
                identity.child_oid,
            ))
        {
            return Err(mismatch());
        }
        let updated = sqlx::query(
            "UPDATE retention_partition_retirements \
             SET lease_owner = $2, \
                 lease_expires_at = statement_timestamp() + INTERVAL '5 minutes' \
             WHERE parent_table = 'retained_response_objects' \
               AND partition_table = $1 AND completed_at IS NULL",
        )
        .bind(&identity.child)
        .bind(owner)
        .execute(&mut *transaction)
        .await
        .map_err(retirement_database_error)?;
        if updated.rows_affected() != 1 {
            return Err(mismatch());
        }
    } else {
        // Candidate discovery above is intentionally unlocked: the movement
        // path takes partition advisory lock before bucket row lock. Match
        // that global order here, then reselect and fence the exact generation
        // under the row lock before journaling.
        let bucket: Option<(String, String, String, Oid)> = sqlx::query_as(
            "SELECT state, partition_schema, partition_table, partition_oid \
             FROM retained_response_buckets WHERE delete_on = $1 FOR UPDATE",
        )
        .bind(identity.lower)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retirement_database_error)?;
        if bucket
            != Some((
                "active".to_string(),
                identity.schema.clone(),
                identity.child.clone(),
                identity.child_oid,
            ))
        {
            return Err(mismatch());
        }
        let inserted = sqlx::query(
            r#"
            INSERT INTO retention_partition_retirements (
                parent_table, partition_table, partition_oid,
                partition_schema, partition_schema_oid, parent_oid,
                lower_bound, upper_bound, lease_owner, lease_expires_at
            ) VALUES (
                'retained_response_objects', $1, $2::oid,
                $3, $4::oid, $5::oid, $6, $7, $8,
                statement_timestamp() + INTERVAL '5 minutes'
            )
            "#,
        )
        .bind(&identity.child)
        .bind(identity.child_oid)
        .bind(&identity.schema)
        .bind(identity.schema_oid)
        .bind(identity.parent_oid)
        .bind(identity.lower)
        .bind(identity.upper)
        .bind(owner)
        .execute(&mut *transaction)
        .await
        .map_err(retirement_database_error)?;
        if inserted.rows_affected() != 1 {
            return Err(mismatch());
        }
        let fenced = sqlx::query(
            "UPDATE retained_response_buckets \
             SET state = 'retiring', state_changed_at = statement_timestamp() \
             WHERE delete_on = $1 AND state = 'active' \
               AND partition_schema = $2 AND partition_table = $3 \
               AND partition_oid = $4::oid",
        )
        .bind(identity.lower)
        .bind(&identity.schema)
        .bind(&identity.child)
        .bind(identity.child_oid)
        .execute(&mut *transaction)
        .await
        .map_err(retirement_database_error)?;
        if fenced.rows_affected() != 1 {
            return Err(mismatch());
        }
    }

    transaction
        .commit()
        .await
        .map_err(retirement_database_error)?;
    Ok(Claim::Claimed(identity))
}

async fn finish(
    connection: &mut PgConnection,
    identity: &Identity,
    owner: Uuid,
) -> Result<RetainedResponseRetirementOutcome> {
    if inspect_identity(&mut *connection, identity).await? != Attachment::Detached {
        return Err(mismatch());
    }
    let mut transaction = connection
        .begin()
        .await
        .map_err(retirement_database_error)?;
    if !lock_parent(&mut transaction).await?
        || !lock_partition(&mut transaction, identity.lower).await?
    {
        transaction
            .rollback()
            .await
            .map_err(retirement_database_error)?;
        return Ok(RetainedResponseRetirementOutcome::Retryable);
    }
    let lock_statement = format!(
        "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
        qualified(&identity.schema, &identity.child)
    );
    if let Err(lock_error) = sqlx::query(&lock_statement)
        .execute(&mut *transaction)
        .await
    {
        let retryable = is_retryable(&lock_error);
        transaction
            .rollback()
            .await
            .map_err(retirement_database_error)?;
        return if retryable {
            Ok(RetainedResponseRetirementOutcome::Retryable)
        } else {
            Err(failed())
        };
    }
    if inspect_identity(&mut *transaction, identity).await? != Attachment::Detached {
        return Err(mismatch());
    }

    let locked: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM retention_partition_retirements journal
            JOIN retained_response_buckets bucket
              ON bucket.delete_on = journal.lower_bound
            WHERE journal.parent_table = 'retained_response_objects'
              AND journal.partition_table = $1
              AND journal.partition_oid = $2::oid
              AND journal.partition_schema = $3
              AND journal.partition_schema_oid = $4::oid
              AND journal.parent_oid = $5::oid
              AND journal.lower_bound = $6
              AND journal.upper_bound = $7
              AND journal.completed_at IS NULL
              AND journal.lease_owner = $8
              AND bucket.state = 'retiring'
              AND bucket.partition_schema = journal.partition_schema
              AND bucket.partition_table = journal.partition_table
              AND bucket.partition_oid = journal.partition_oid
            FOR UPDATE OF journal, bucket
        )
        "#,
    )
    .bind(&identity.child)
    .bind(identity.child_oid)
    .bind(&identity.schema)
    .bind(identity.schema_oid)
    .bind(identity.parent_oid)
    .bind(identity.lower)
    .bind(identity.upper)
    .bind(owner)
    .fetch_one(&mut *transaction)
    .await
    .map_err(retirement_database_error)?;
    if !locked {
        return Err(mismatch());
    }

    let drop_statement = format!(
        "DROP TABLE {}",
        qualified(&identity.schema, &identity.child)
    );
    if let Err(drop_error) = sqlx::query(&drop_statement)
        .execute(&mut *transaction)
        .await
    {
        let retryable = is_retryable(&drop_error);
        transaction
            .rollback()
            .await
            .map_err(retirement_database_error)?;
        return if retryable {
            Ok(RetainedResponseRetirementOutcome::Retryable)
        } else {
            Err(failed())
        };
    }

    let completed: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            r#"
            WITH completion AS (
                SELECT clock_timestamp() AS completed_at
            ), bucket_update AS (
                UPDATE retained_response_buckets bucket
                SET state = 'retired', state_changed_at = completion.completed_at
                FROM completion
                WHERE bucket.delete_on = $1
                  AND bucket.state = 'retiring'
                  AND bucket.partition_schema = $2
                  AND bucket.partition_table = $3
                  AND bucket.partition_oid = $4::oid
                RETURNING bucket.state_changed_at
            ), journal_update AS (
                UPDATE retention_partition_retirements journal
                SET completed_at = completion.completed_at,
                    lease_owner = NULL,
                    lease_expires_at = NULL
                FROM completion
                WHERE journal.parent_table = 'retained_response_objects'
                  AND journal.partition_table = $3
                  AND journal.partition_oid = $4::oid
                  AND journal.lease_owner = $5
                  AND journal.completed_at IS NULL
                RETURNING journal.completed_at
            )
            SELECT bucket_update.state_changed_at, journal_update.completed_at
            FROM bucket_update CROSS JOIN journal_update
            "#,
        )
        .bind(identity.lower)
        .bind(&identity.schema)
        .bind(&identity.child)
        .bind(identity.child_oid)
        .bind(owner)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retirement_database_error)?;
    let Some((bucket_time, journal_time)) = completed else {
        return Err(mismatch());
    };
    if bucket_time != journal_time {
        return Err(mismatch());
    }
    transaction
        .commit()
        .await
        .map_err(retirement_database_error)?;
    Ok(RetainedResponseRetirementOutcome::Retired)
}

async fn lock_exact_names(
    transaction: &mut Transaction<'_, Postgres>,
    identity: &Identity,
) -> Result<Attachment> {
    configure_guard(transaction).await?;
    let parent_lock = format!(
        "LOCK TABLE {} IN ACCESS SHARE MODE",
        qualified(&identity.schema, PARENT)
    );
    sqlx::query(&parent_lock)
        .execute(&mut **transaction)
        .await
        .map_err(retirement_database_error)?;
    let child_lock = format!(
        "LOCK TABLE {} IN ACCESS SHARE MODE",
        qualified(&identity.schema, &identity.child)
    );
    sqlx::query(&child_lock)
        .execute(&mut **transaction)
        .await
        .map_err(retirement_database_error)?;
    inspect_identity(&mut **transaction, identity).await
}

async fn detach_attached<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    connection: &mut PgConnection,
    identity: &Identity,
) -> Result<DetachProgress> {
    // The guard locks are intentionally held by the attested ordinary primary
    // while PostgreSQL phase 1 resolves the textual DDL names. Reinspection
    // under those locks proves the names still denote the journaled OIDs; no
    // rename/drop/recreate can substitute another relation before phase 1
    // saves those OIDs for its concurrent phase 2.
    let mut guard = manager.begin_write().await.map_err(|_| failed())?;
    match lock_exact_names(&mut guard, identity).await {
        Ok(Attachment::Attached) => {}
        Ok(_) => {
            guard.rollback().await.map_err(retirement_database_error)?;
            return Err(mismatch());
        }
        Err(error) => {
            guard.rollback().await.map_err(retirement_database_error)?;
            return Err(error);
        }
    }

    let statement = format!(
        "ALTER TABLE {} DETACH PARTITION {} CONCURRENTLY",
        qualified(&identity.schema, PARENT),
        qualified(&identity.schema, &identity.child)
    );
    let query = sqlx::query(&statement);
    let detach = query.execute(&mut *connection);
    tokio::pin!(detach);

    loop {
        tokio::select! {
            result = &mut detach => {
                guard.rollback().await.map_err(retirement_database_error)?;
                return match result {
                    Ok(_) => Ok(DetachProgress::Detached),
                    Err(error) if is_retryable(&error) => Ok(DetachProgress::Retryable),
                    Err(_) => Err(failed()),
                };
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                match inspect_identity(&mut *guard, identity).await {
                    Ok(Attachment::Attached) => continue,
                    Ok(Attachment::DetachPending) => break,
                    Ok(Attachment::Detached) => break,
                    Err(error) => {
                        let rollback = guard.rollback().await;
                        let ddl_result = detach.await;
                        rollback.map_err(retirement_database_error)?;
                        if let Err(ddl_error) = ddl_result
                            && is_retryable(&ddl_error)
                        {
                            return Ok(DetachProgress::Retryable);
                        }
                        return Err(error);
                    }
                }
            }
        }
    }

    let guard_result = guard.commit().await;
    let ddl_result = detach.await;
    guard_result.map_err(retirement_database_error)?;
    match ddl_result {
        Ok(_) => Ok(DetachProgress::Detached),
        Err(error) if is_retryable(&error) => Ok(DetachProgress::Retryable),
        Err(_) => Err(failed()),
    }
}

async fn finalize_pending(
    connection: &mut PgConnection,
    identity: &Identity,
) -> Result<DetachProgress> {
    let mut transaction = connection
        .begin()
        .await
        .map_err(retirement_database_error)?;
    match lock_exact_names(&mut transaction, identity).await {
        Ok(Attachment::DetachPending) => {}
        Ok(_) => {
            transaction
                .rollback()
                .await
                .map_err(retirement_database_error)?;
            return Err(mismatch());
        }
        Err(error) => {
            transaction
                .rollback()
                .await
                .map_err(retirement_database_error)?;
            return Err(error);
        }
    }
    let statement = format!(
        "ALTER TABLE {} DETACH PARTITION {} FINALIZE",
        qualified(&identity.schema, PARENT),
        qualified(&identity.schema, &identity.child)
    );
    match sqlx::query(&statement).execute(&mut *transaction).await {
        Ok(_) => {}
        Err(error) if is_retryable(&error) => {
            transaction
                .rollback()
                .await
                .map_err(retirement_database_error)?;
            return Ok(DetachProgress::Retryable);
        }
        Err(_) => {
            transaction
                .rollback()
                .await
                .map_err(retirement_database_error)?;
            return Err(failed());
        }
    }
    match inspect_identity(&mut *transaction, identity).await {
        Ok(Attachment::Detached) => {}
        Ok(_) => {
            transaction
                .rollback()
                .await
                .map_err(retirement_database_error)?;
            return Err(mismatch());
        }
        Err(error) => {
            transaction
                .rollback()
                .await
                .map_err(retirement_database_error)?;
            return Err(error);
        }
    }
    transaction
        .commit()
        .await
        .map_err(retirement_database_error)?;
    Ok(DetachProgress::Detached)
}

async fn detach_and_finish<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    connection: &mut PgConnection,
    identity: &Identity,
    owner: Uuid,
) -> Result<RetainedResponseRetirementOutcome> {
    let attachment = inspect_identity(&mut *connection, identity).await?;
    let progress = match attachment {
        Attachment::Attached => detach_attached(manager, connection, identity).await?,
        Attachment::DetachPending => finalize_pending(connection, identity).await?,
        Attachment::Detached => DetachProgress::Detached,
    };
    if progress == DetachProgress::Retryable {
        return Ok(RetainedResponseRetirementOutcome::Retryable);
    }
    finish(connection, identity, owner).await
}

pub(super) async fn retire_expired_response_partition<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    select_new: bool,
) -> Result<RetainedResponseRetirementOutcome> {
    let pool = manager
        .partition_maintenance_pool
        .as_ref()
        .filter(|_| manager.partition_maintenance_attested)
        .ok_or_else(|| error(RetainedResponseMaintenanceError::PartitionMaintenancePoolMissing))?;
    let mut connection = pool.acquire().await.map_err(retirement_database_error)?;
    configure_session(&mut connection).await?;
    let claim = match claim(
        &mut connection,
        manager.partition_maintenance_lease_owner,
        select_new,
    )
    .await
    {
        Err(error) if is_contention(&error) => {
            return Ok(RetainedResponseRetirementOutcome::Retryable);
        }
        Err(error) => return Err(error),
        Ok(claim) => claim,
    };
    let identity = match claim {
        Claim::None => return Ok(RetainedResponseRetirementOutcome::NoCandidate),
        Claim::Contended => return Ok(RetainedResponseRetirementOutcome::Retryable),
        Claim::Claimed(identity) => identity,
    };
    match detach_and_finish(
        manager,
        &mut connection,
        &identity,
        manager.partition_maintenance_lease_owner,
    )
    .await
    {
        Err(error) if is_contention(&error) => Ok(RetainedResponseRetirementOutcome::Retryable),
        result => result,
    }
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
    let fence_seconds = manager
        .retained_response_fence_seconds()
        .ok_or_else(|| error(RetainedResponseMaintenanceError::FencePolicyMissing))?;
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
