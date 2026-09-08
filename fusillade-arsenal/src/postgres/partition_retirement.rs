//! Crash-safe, journaled partition retirement shared by every time-keyed
//! content family (daily retained responses, weekly batch archive, weekly
//! template generations).
//!
//! One engine, one state machine: claim (resume the oldest unfinished
//! journal, else fence a newly eligible bucket), OID-exact identity
//! inspection, `DETACH PARTITION CONCURRENTLY`, `FINALIZE` recovery, drop of
//! only the journaled relation, and atomic completion. Families differ only
//! in their declarative [`FamilySpec`]: names, bounds width, eligibility
//! SQL, and an optional metadata statement committed with the completion
//! timestamp.

use crate::error::{FusilladeError, Result};
use crate::manager::{RetainedResponseMaintenanceError, RetainedResponseRetirementOutcome};
use crate::{PoolProvider, PostgresRequestManager};
use chrono::NaiveDate;
use sqlx::postgres::types::Oid;
use sqlx::{Connection, Executor, PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

/// Declarative description of one retirement family. Every string is a
/// compile-time constant owned by the family module; nothing user-supplied
/// ever reaches the composed SQL.
pub(super) struct FamilySpec {
    /// Partitioned parent relation name.
    pub parent: &'static str,
    /// Lifecycle bucket registry table.
    pub bucket_table: &'static str,
    /// The bucket registry's date primary-key column.
    pub bucket_key: &'static str,
    /// Partition width in days (1 = daily, 7 = weekly).
    pub bounds_days: i32,
    /// Whether a lower bound is valid for this family (weekly families
    /// require an ISO Monday).
    pub valid_lower: fn(NaiveDate) -> bool,
    /// Canonical child relation name for a lower bound.
    pub canonical_child: fn(NaiveDate) -> String,
    /// Boolean query proving no bucket is fenced/retired without its exact
    /// journal counterpart. No binds.
    pub orphan_fence_sql: &'static str,
    /// Candidate query returning one eligible bucket as
    /// (partition_schema, partition_schema_oid, parent_oid, partition_table,
    /// partition_oid, lower_bound, upper_bound). Binds `$1 = retention days`
    /// when `candidate_binds_retention`.
    pub candidate_sql: &'static str,
    pub candidate_binds_retention: bool,
    /// Optional metadata statement committed atomically with journal/bucket
    /// completion. Binds: $1 lower bound, $2 upper bound, $3 completion
    /// timestamp. Must touch only content-free metadata.
    pub completion_sql: Option<&'static str>,
}

impl FamilySpec {
    fn partition_lock_expression(&self) -> String {
        format!(
            "hashtextextended('{}.partition:' || current_schema() || ':' \
                 || to_char($1::date, 'YYYYMMDD'), 0)",
            self.parent
        )
    }
}

#[derive(Debug, Clone)]
pub(super) struct Identity {
    pub schema: String,
    pub schema_oid: Oid,
    pub parent_oid: Oid,
    pub child: String,
    pub child_oid: Oid,
    pub lower: NaiveDate,
    pub upper: NaiveDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attachment {
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
        formatter.write_str("Partition retirement was contended")
    }
}

impl std::error::Error for RetirementContention {}

fn error(kind: RetainedResponseMaintenanceError) -> FusilladeError {
    kind.into_fusillade_error()
}

pub(super) fn failed() -> FusilladeError {
    error(RetainedResponseMaintenanceError::RetirementFailed)
}

fn mismatch() -> FusilladeError {
    error(RetainedResponseMaintenanceError::RetirementIdentityMismatch)
}

pub(super) fn is_retryable(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if matches!(database.code().as_deref(), Some("55P03" | "57014"))
    )
}

pub(super) fn retirement_database_error(error: sqlx::Error) -> FusilladeError {
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

async fn lock_parent(
    transaction: &mut Transaction<'_, Postgres>,
    family: &FamilySpec,
) -> Result<bool> {
    let statement = format!(
        "SELECT pg_try_advisory_xact_lock(\
             hashtextextended('{}.retirement:' || current_schema(), 0)\
         )",
        family.parent
    );
    sqlx::query_scalar(&statement)
        .fetch_one(&mut **transaction)
        .await
        .map_err(retirement_database_error)
}

async fn lock_partition(
    transaction: &mut Transaction<'_, Postgres>,
    family: &FamilySpec,
    lower: NaiveDate,
) -> Result<bool> {
    let statement = format!(
        "SELECT pg_try_advisory_xact_lock({})",
        family.partition_lock_expression()
    );
    sqlx::query_scalar(&statement)
        .bind(lower)
        .fetch_one(&mut **transaction)
        .await
        .map_err(retirement_database_error)
}

pub(super) async fn inspect_identity<'e, E>(
    executor: E,
    family: &FamilySpec,
    identity: &Identity,
) -> Result<Attachment>
where
    E: Executor<'e, Database = Postgres>,
{
    if identity.upper != identity.lower + chrono::Duration::days(family.bounds_days.into())
        || !(family.valid_lower)(identity.lower)
        || identity.child != (family.canonical_child)(identity.lower)
    {
        return Err(mismatch());
    }

    let statement = r#"
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
        "#;
    let row = sqlx::query(statement)
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
            .is_ok_and(|value| value == family.parent)
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

async fn claim(
    connection: &mut PgConnection,
    family: &FamilySpec,
    owner: Uuid,
    select_new: bool,
    retention_days: Option<i32>,
) -> Result<Claim> {
    let mut transaction = connection
        .begin()
        .await
        .map_err(retirement_database_error)?;
    if !lock_parent(&mut transaction, family).await? {
        transaction
            .rollback()
            .await
            .map_err(retirement_database_error)?;
        return Ok(Claim::Contended);
    }

    let unfinished_statement = format!(
        r#"
        SELECT partition_schema, partition_schema_oid,
               parent_oid, partition_table, partition_oid,
               lower_bound, upper_bound, lease_owner,
               COALESCE(lease_expires_at > statement_timestamp(), FALSE) AS lease_live
        FROM retention_partition_retirements
        WHERE parent_table = '{}'
          AND completed_at IS NULL
        ORDER BY requested_at, partition_table
        FOR UPDATE
        LIMIT 1
        "#,
        family.parent
    );
    let unfinished = sqlx::query(&unfinished_statement)
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
        let orphan_fence: bool = sqlx::query_scalar(family.orphan_fence_sql)
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
        let candidate_query = sqlx::query(family.candidate_sql);
        let candidate_query = if family.candidate_binds_retention {
            candidate_query.bind(retention_days.ok_or_else(failed)?)
        } else {
            candidate_query
        };
        let Some(row) = candidate_query
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

    if !lock_partition(&mut transaction, family, identity.lower).await? {
        transaction
            .rollback()
            .await
            .map_err(retirement_database_error)?;
        return Ok(Claim::Contended);
    }
    let attachment = inspect_identity(&mut *transaction, family, &identity).await?;
    if !recovering && attachment != Attachment::Attached {
        return Err(mismatch());
    }

    let bucket_statement = format!(
        "SELECT state, partition_schema, partition_table, partition_oid \
         FROM {} WHERE {} = $1 FOR UPDATE",
        family.bucket_table, family.bucket_key
    );
    let bucket: Option<(String, String, String, Oid)> = sqlx::query_as(&bucket_statement)
        .bind(identity.lower)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retirement_database_error)?;
    let expected_state = if recovering { "retiring" } else { "active" };
    if bucket
        != Some((
            expected_state.to_string(),
            identity.schema.clone(),
            identity.child.clone(),
            identity.child_oid,
        ))
    {
        return Err(mismatch());
    }

    if recovering {
        let renew_statement = format!(
            "UPDATE retention_partition_retirements \
             SET lease_owner = $2, \
                 lease_expires_at = statement_timestamp() + INTERVAL '5 minutes' \
             WHERE parent_table = '{}' \
               AND partition_table = $1 AND completed_at IS NULL",
            family.parent
        );
        let updated = sqlx::query(&renew_statement)
            .bind(&identity.child)
            .bind(owner)
            .execute(&mut *transaction)
            .await
            .map_err(retirement_database_error)?;
        if updated.rows_affected() != 1 {
            return Err(mismatch());
        }
    } else {
        let journal_statement = format!(
            r#"
            INSERT INTO retention_partition_retirements (
                parent_table, partition_table, partition_oid,
                partition_schema, partition_schema_oid, parent_oid,
                lower_bound, upper_bound, lease_owner, lease_expires_at
            ) VALUES (
                '{}', $1, $2::oid,
                $3, $4::oid, $5::oid, $6, $7, $8,
                statement_timestamp() + INTERVAL '5 minutes'
            )
            "#,
            family.parent
        );
        let inserted = sqlx::query(&journal_statement)
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
        let fence_statement = format!(
            "UPDATE {} \
             SET state = 'retiring', state_changed_at = statement_timestamp() \
             WHERE {} = $1 AND state = 'active' \
               AND partition_schema = $2 AND partition_table = $3 \
               AND partition_oid = $4::oid",
            family.bucket_table, family.bucket_key
        );
        let fenced = sqlx::query(&fence_statement)
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
    family: &FamilySpec,
    identity: &Identity,
    owner: Uuid,
) -> Result<RetainedResponseRetirementOutcome> {
    if inspect_identity(&mut *connection, family, identity).await? != Attachment::Detached {
        return Err(mismatch());
    }
    let mut transaction = connection
        .begin()
        .await
        .map_err(retirement_database_error)?;
    if !lock_parent(&mut transaction, family).await?
        || !lock_partition(&mut transaction, family, identity.lower).await?
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
    if inspect_identity(&mut *transaction, family, identity).await? != Attachment::Detached {
        return Err(mismatch());
    }

    let lock_pair_statement = format!(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM retention_partition_retirements journal
            JOIN {bucket} bucket
              ON bucket.{key} = journal.lower_bound
            WHERE journal.parent_table = '{parent}'
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
        bucket = family.bucket_table,
        key = family.bucket_key,
        parent = family.parent,
    );
    let locked: bool = sqlx::query_scalar(&lock_pair_statement)
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

    let completion_statement = format!(
        r#"
        WITH completion AS (
            SELECT clock_timestamp() AS completed_at
        ), bucket_update AS (
            UPDATE {bucket} bucket
            SET state = 'retired', state_changed_at = completion.completed_at
            FROM completion
            WHERE bucket.{key} = $1
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
            WHERE journal.parent_table = '{parent}'
              AND journal.partition_table = $3
              AND journal.partition_oid = $4::oid
              AND journal.lease_owner = $5
              AND journal.completed_at IS NULL
            RETURNING journal.completed_at
        )
        SELECT bucket_update.state_changed_at, journal_update.completed_at
        FROM bucket_update CROSS JOIN journal_update
        "#,
        bucket = family.bucket_table,
        key = family.bucket_key,
        parent = family.parent,
    );
    let completed: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(&completion_statement)
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

    if let Some(completion_sql) = family.completion_sql {
        sqlx::query(completion_sql)
            .bind(identity.lower)
            .bind(identity.upper)
            .bind(bucket_time)
            .execute(&mut *transaction)
            .await
            .map_err(retirement_database_error)?;
    }

    transaction
        .commit()
        .await
        .map_err(retirement_database_error)?;
    Ok(RetainedResponseRetirementOutcome::Retired)
}

async fn lock_exact_names(
    transaction: &mut Transaction<'_, Postgres>,
    family: &FamilySpec,
    identity: &Identity,
) -> Result<Attachment> {
    configure_guard(transaction).await?;
    let parent_lock = format!(
        "LOCK TABLE {} IN ACCESS SHARE MODE",
        qualified(&identity.schema, family.parent)
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
    inspect_identity(&mut **transaction, family, identity).await
}

async fn detach_attached<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    connection: &mut PgConnection,
    family: &FamilySpec,
    identity: &Identity,
) -> Result<DetachProgress> {
    // The guard locks are intentionally held by the attested ordinary primary
    // while PostgreSQL phase 1 resolves the textual DDL names. Reinspection
    // under those locks proves the names still denote the journaled OIDs; no
    // rename/drop/recreate can substitute another relation before phase 1
    // saves those OIDs for its concurrent phase 2.
    let mut guard = manager.begin_write().await.map_err(|_| failed())?;
    match lock_exact_names(&mut guard, family, identity).await {
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
        qualified(&identity.schema, family.parent),
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
                match inspect_identity(&mut *guard, family, identity).await {
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
    family: &FamilySpec,
    identity: &Identity,
) -> Result<DetachProgress> {
    let mut transaction = connection
        .begin()
        .await
        .map_err(retirement_database_error)?;
    match lock_exact_names(&mut transaction, family, identity).await {
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
        qualified(&identity.schema, family.parent),
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
    match inspect_identity(&mut *transaction, family, identity).await {
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
    family: &FamilySpec,
    identity: &Identity,
    owner: Uuid,
) -> Result<RetainedResponseRetirementOutcome> {
    let attachment = inspect_identity(&mut *connection, family, identity).await?;
    let progress = match attachment {
        Attachment::Attached => detach_attached(manager, connection, family, identity).await?,
        Attachment::DetachPending => finalize_pending(connection, family, identity).await?,
        Attachment::Detached => DetachProgress::Detached,
    };
    if progress == DetachProgress::Retryable {
        return Ok(RetainedResponseRetirementOutcome::Retryable);
    }
    finish(connection, family, identity, owner).await
}

/// One retirement tick for a family: resume unfinished work first, then (only
/// when `select_new`) fence and retire at most one newly eligible bucket.
pub(super) async fn retire<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    family: &FamilySpec,
    select_new: bool,
    retention_days: Option<i32>,
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
        family,
        manager.partition_maintenance_lease_owner,
        select_new,
        retention_days,
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
        family,
        &identity,
        manager.partition_maintenance_lease_owner,
    )
    .await
    {
        Err(error) if is_contention(&error) => Ok(RetainedResponseRetirementOutcome::Retryable),
        result => result,
    }
}
