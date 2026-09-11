//! Recovery for explicitly managed indexes.
//!
//! `CREATE INDEX CONCURRENTLY` cannot run inside a transaction, so SQLx applies
//! those migrations with `-- no-transaction` and records them only after the
//! statement returns. An interrupted build (cancelled backend, lost connection,
//! killed process) leaves an INVALID index behind and *no* migration row. On the
//! next run `IF NOT EXISTS` sees the invalid index, skips the build, and SQLx
//! records the migration as applied. Every later validation then fails and no
//! amount of retrying helps: the migration history says the index exists, and
//! the catalog says it is unusable. That is exactly what happened to
//! `idx_batches_owner_created_at_id` during the 11.9.1 rollout.
//!
//! This module fixes that class of failure without editing released migrations.
//! Before the migrator runs, every managed index is inspected in `pg_index` and
//! classified as absent, valid, invalid, or defined differently from what the
//! migration intended. Invalid indexes with the intended definition are rebuilt
//! with `REINDEX INDEX CONCURRENTLY`; absent indexes whose creating migration is
//! already recorded are built concurrently; anything else is reported and left
//! untouched. After the migrator runs, the same inspection must find every
//! managed index valid, otherwise the run fails.
//!
//! Rules this code follows deliberately:
//!
//! * `IF NOT EXISTS` is never taken as proof that an index is usable.
//! * An index with a different definition is never dropped or replaced. That is
//!   an operator decision: the definition mismatch is reported with both sides.
//! * Only leftovers of *our own* concurrent rebuilds (`<name>_ccnew*`,
//!   `<name>_ccold*`, invalid) are removed, because Postgres documents dropping
//!   them as the recovery procedure for an interrupted `REINDEX CONCURRENTLY`.
//! * Application queries are never cancelled; every step takes at most the
//!   locks a concurrent build takes, bounded by `lock_timeout`.
//! * Statistics are refreshed (`ANALYZE`) on any table whose index was built or
//!   rebuilt, so the planner sees the new index immediately.
//!
//! Partitioned tables need a different procedure because `CREATE INDEX
//! CONCURRENTLY` is not supported on a partitioned parent: the parent index is
//! created `ON ONLY` (metadata), each leaf partition gets its own concurrent
//! build, and the children are attached. The parent becomes valid once every
//! partition has an attached valid child. That mirrors
//! `scripts/prepare_retained_*_index.sql`, which populated installations
//! previously had to run by hand.

use std::collections::HashSet;

use sqlx::PgPool;
use sqlx::postgres::PgConnection;
use sqlx::postgres::types::Oid;
use tracing::{info, warn};

/// How an index is built and repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// An ordinary table: built and repaired with `CONCURRENTLY`.
    Plain,
    /// A partitioned parent: the parent index is metadata (`ON ONLY`), every
    /// leaf partition gets a concurrent child build, and children are attached.
    Partitioned {
        /// Prefix for child index names; the partition's OID is appended so
        /// names stay stable across retries (matches the prepare scripts).
        child_prefix: &'static str,
    },
}

/// An index whose lifecycle this module owns.
#[derive(Debug, Clone, Copy)]
pub struct ManagedIndex {
    /// Index name, unqualified. Resolved in `current_schema()`.
    pub name: &'static str,
    /// Table name, unqualified. Resolved in `current_schema()`.
    pub table: &'static str,
    /// The definition exactly as `pg_get_indexdef` deparses it after
    /// `ON <table> `, for example
    /// `USING btree (created_by, created_at DESC, id DESC) WHERE (deleted_at IS NULL)`.
    /// Doubles as the SQL used to (re)build the index. A test pins each entry
    /// to the real catalog output so drift is caught at review time.
    pub definition: &'static str,
    /// Version of the migration that creates the index. An absent index whose
    /// creating migration is already recorded is built here, because that is
    /// the signature of an interrupted build followed by an `IF NOT EXISTS`
    /// retry — or of an index dropped by hand.
    pub created_by_migration: i64,
    pub kind: IndexKind,
}

/// What the catalog says about a managed index right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    /// No relation of that name in the schema.
    Absent,
    /// Present, valid, ready, and defined as intended.
    Valid,
    /// Present with the intended definition but `indisvalid = false` (an
    /// interrupted concurrent build or rebuild).
    Invalid,
    /// Present but not the index the migration intended. Never repaired
    /// automatically.
    WrongDefinition { expected: String, actual: String },
}

/// What was done to one managed index during a repair pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// Nothing to do: valid, or absent with its migration still pending.
    Untouched(IndexState),
    /// `REINDEX INDEX CONCURRENTLY` succeeded.
    Reindexed,
    /// Built from scratch because the creating migration is already recorded.
    Built,
    /// Partitioned: number of child indexes built, rebuilt or attached.
    PartitionsRepaired {
        built: usize,
        reindexed: usize,
        attached: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairReport {
    pub index: &'static str,
    pub action: RepairAction,
    /// Names of invalid `_ccnew`/`_ccold` leftovers that were dropped.
    pub dropped_leftovers: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedIndexError {
    #[error(
        "index {name} on {table} has the wrong definition; expected `{expected}` but found `{actual}`. \
             It was not modified. Rename or drop it by hand after confirming nothing else depends on it, then rerun the migration"
    )]
    WrongDefinition {
        name: &'static str,
        table: &'static str,
        expected: String,
        actual: String,
    },
    #[error(
        "index {name} on {table} is {state:?} after repair; a concurrent build or rebuild did not complete. \
             Rerun the migration; if this persists check pg_stat_activity for a stuck build and the server log"
    )]
    StillUnusable {
        name: &'static str,
        table: &'static str,
        state: IndexState,
    },
    #[error(
        "index {name} on {table} is missing although migration {version} is recorded as applied; the build could not be repeated"
    )]
    MissingAfterMigration {
        name: &'static str,
        table: &'static str,
        version: i64,
    },
    #[error(
        "partition {partition} of {table} has no valid attached child for index {name} after repair"
    )]
    PartitionIncomplete {
        name: &'static str,
        table: &'static str,
        partition: String,
    },
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

/// The managed indexes of the Fusillade schema, in dependency order.
///
/// Add an entry here for every index a migration builds with `CONCURRENTLY`
/// or that populated installations must prebuild. Keep `definition` equal to
/// what `pg_get_indexdef` prints; `managed_index_definitions_match_catalog`
/// fails otherwise.
pub fn managed_indexes() -> &'static [ManagedIndex] {
    &[
        ManagedIndex {
            name: "idx_batches_unfrozen_sweep",
            table: "batches",
            definition: "USING btree (cancelled_at) WHERE ((counts_frozen_at IS NULL) AND (deleted_at IS NULL))",
            created_by_migration: 20260908000000,
            kind: IndexKind::Plain,
        },
        ManagedIndex {
            name: "idx_retained_response_objects_state_terminal",
            table: "retained_response_objects",
            definition: "USING btree (state, terminal_at) WHERE (object_kind = 'request'::text)",
            created_by_migration: 20260909000000,
            kind: IndexKind::Partitioned {
                child_prefix: "retained_terminal_",
            },
        },
        ManagedIndex {
            name: "idx_retained_response_objects_created",
            table: "retained_response_objects",
            definition: "USING btree (created_at DESC, object_id DESC) WHERE (object_kind = 'request'::text)",
            created_by_migration: 20260910000000,
            kind: IndexKind::Partitioned {
                child_prefix: "retained_created_",
            },
        },
        ManagedIndex {
            name: "idx_batches_owner_created_at_id",
            table: "batches",
            definition: "USING btree (created_by, created_at DESC, id DESC) WHERE (deleted_at IS NULL)",
            created_by_migration: 20260910010000,
            kind: IndexKind::Plain,
        },
    ]
}

/// Quote an identifier for interpolation into DDL.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[derive(Debug, sqlx::FromRow)]
struct CatalogIndex {
    indisvalid: bool,
    indisready: bool,
    /// `i` for a plain index, `I` for a partitioned parent index.
    relkind: String,
    indexdef: String,
    /// `pg_get_indexdef` qualifies the table when it is not on the search
    /// path; the heap name is compared unqualified.
    heap_name: String,
}

const CATALOG_QUERY: &str = r#"
    SELECT i.indisvalid,
           i.indisready,
           c.relkind::text AS relkind,
           pg_get_indexdef(i.indexrelid) AS indexdef,
           h.relname AS heap_name
    FROM pg_index i
    JOIN pg_class c ON c.oid = i.indexrelid
    JOIN pg_class h ON h.oid = i.indrelid
    WHERE i.indexrelid = to_regclass(format('%I.%I', current_schema(), $1))
"#;

async fn catalog_index(
    conn: &mut PgConnection,
    name: &str,
) -> Result<Option<CatalogIndex>, sqlx::Error> {
    sqlx::query_as::<_, CatalogIndex>(CATALOG_QUERY)
        .bind(name)
        .fetch_optional(conn)
        .await
}

/// Split `pg_get_indexdef` output into `(unique, index name, table, tail)`.
///
/// The deparsed form is
/// `CREATE [UNIQUE ]INDEX <name> ON [<schema>.]<table> USING <am> (...) [WHERE (...)]`.
fn split_indexdef(indexdef: &str) -> Option<(bool, &str, &str, &str)> {
    let (head, tail) = indexdef.split_once(" USING ")?;
    let unique = head.starts_with("CREATE UNIQUE INDEX ");
    let rest = head
        .strip_prefix("CREATE UNIQUE INDEX ")
        .or_else(|| head.strip_prefix("CREATE INDEX "))?;
    let (name, table) = rest.split_once(" ON ")?;
    let table = table.rsplit('.').next().unwrap_or(table);
    Some((unique, name, table, tail))
}

/// Normalise a deparsed tail for comparison: everything after `USING `.
fn definition_tail(indexdef: &str) -> Option<String> {
    let (unique, _, _, tail) = split_indexdef(indexdef)?;
    Some(if unique {
        format!("UNIQUE {tail}")
    } else {
        format!("USING {tail}")
    })
}

fn classify(spec: &ManagedIndex, row: Option<&CatalogIndex>) -> IndexState {
    let Some(row) = row else {
        return IndexState::Absent;
    };
    let actual = definition_tail(&row.indexdef).unwrap_or_else(|| row.indexdef.clone());
    let expected_kind = match spec.kind {
        IndexKind::Plain => "i",
        IndexKind::Partitioned { .. } => "I",
    };
    let heap_matches = row.heap_name == spec.table;
    if !heap_matches || row.relkind != expected_kind || actual != spec.definition {
        let actual = if heap_matches {
            actual
        } else {
            format!("{actual} (on table {})", row.heap_name)
        };
        return IndexState::WrongDefinition {
            expected: spec.definition.to_string(),
            actual,
        };
    }
    if row.indisvalid && row.indisready {
        IndexState::Valid
    } else {
        IndexState::Invalid
    }
}

/// Inspect one managed index in `current_schema()`.
pub async fn inspect(
    conn: &mut PgConnection,
    spec: &ManagedIndex,
) -> Result<IndexState, sqlx::Error> {
    let row = catalog_index(conn, spec.name).await?;
    Ok(classify(spec, row.as_ref()))
}

/// Inspect every managed index and return the ones that are not valid.
pub async fn inspect_all(
    pool: &PgPool,
    specs: &[ManagedIndex],
) -> Result<Vec<(&'static str, IndexState)>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let mut states = Vec::new();
    for spec in specs {
        let state = inspect(&mut conn, spec).await?;
        if state != IndexState::Valid {
            states.push((spec.name, state));
        }
    }
    Ok(states)
}

/// Session settings for repair work: never let a role-level
/// `statement_timeout` cut a long concurrent build short, and bound every lock
/// wait so an unexpected blocker surfaces as an error instead of a hang.
async fn prepare_session(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SET statement_timeout = 0")
        .execute(&mut *conn)
        .await?;
    sqlx::query("SET lock_timeout = '5s'")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Drop invalid `<name>_ccnew*` / `<name>_ccold*` leftovers of an interrupted
/// `REINDEX CONCURRENTLY` on the managed table. Postgres documents dropping
/// them as the recovery step; they are never usable and only ever belong to
/// a rebuild of this index.
async fn drop_rebuild_leftovers(
    conn: &mut PgConnection,
    spec: &ManagedIndex,
) -> Result<Vec<String>, sqlx::Error> {
    let leftovers: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT c.relname
        FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = current_schema()
          AND NOT i.indisvalid
          AND (c.relname ~ ('^' || $1 || '_ccnew[0-9]*$') OR c.relname ~ ('^' || $1 || '_ccold[0-9]*$'))
        ORDER BY c.relname
        "#,
    )
    .bind(regex_escape(spec.name))
    .fetch_all(&mut *conn)
    .await?;
    for name in &leftovers {
        warn!(index = %name, "dropping invalid leftover of an interrupted concurrent rebuild");
        sqlx::query(&format!(
            "DROP INDEX CONCURRENTLY IF EXISTS {}",
            ident(name)
        ))
        .execute(&mut *conn)
        .await?;
    }
    Ok(leftovers)
}

fn regex_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() && c != '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn analyze(conn: &mut PgConnection, table: &str) -> Result<(), sqlx::Error> {
    info!(table, "refreshing planner statistics");
    sqlx::query(&format!("ANALYZE {}", ident(table)))
        .execute(conn)
        .await?;
    Ok(())
}

async fn reindex_concurrently(conn: &mut PgConnection, name: &str) -> Result<(), sqlx::Error> {
    info!(
        index = name,
        "rebuilding invalid index with REINDEX INDEX CONCURRENTLY"
    );
    sqlx::query(&format!("REINDEX INDEX CONCURRENTLY {}", ident(name)))
        .execute(conn)
        .await?;
    Ok(())
}

/// Repair one plain (non-partitioned) managed index.
async fn repair_plain(
    conn: &mut PgConnection,
    spec: &ManagedIndex,
    applied: &HashSet<i64>,
) -> Result<RepairReport, ManagedIndexError> {
    let dropped_leftovers = drop_rebuild_leftovers(conn, spec).await?;
    let state = inspect(conn, spec).await?;
    let action = match state {
        IndexState::Valid => RepairAction::Untouched(IndexState::Valid),
        IndexState::WrongDefinition { expected, actual } => {
            return Err(ManagedIndexError::WrongDefinition {
                name: spec.name,
                table: spec.table,
                expected,
                actual,
            });
        }
        IndexState::Invalid => {
            reindex_concurrently(conn, spec.name).await?;
            analyze(conn, spec.table).await?;
            RepairAction::Reindexed
        }
        IndexState::Absent if applied.contains(&spec.created_by_migration) => {
            info!(
                index = spec.name,
                migration = spec.created_by_migration,
                "index is missing although its migration is recorded; building it concurrently"
            );
            sqlx::query(&format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} {}",
                ident(spec.name),
                ident(spec.table),
                spec.definition
            ))
            .execute(&mut *conn)
            .await?;
            analyze(conn, spec.table).await?;
            RepairAction::Built
        }
        IndexState::Absent => RepairAction::Untouched(IndexState::Absent),
    };
    if !matches!(action, RepairAction::Untouched(_)) {
        let after = inspect(conn, spec).await?;
        if after != IndexState::Valid {
            return Err(ManagedIndexError::StillUnusable {
                name: spec.name,
                table: spec.table,
                state: after,
            });
        }
    }
    Ok(RepairReport {
        index: spec.name,
        action,
        dropped_leftovers,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct PartitionChild {
    partition_oid: Oid,
    partition: String,
    /// Attached child index name (if any).
    attached: Option<String>,
    attached_valid: Option<bool>,
}

/// Repair a partitioned managed index: parent metadata, one concurrent child
/// build per leaf partition, attachment.
///
/// Unlike plain indexes this runs even before the creating migration is
/// recorded: the migration itself refuses to build over populated partitions
/// (it would block writes), so preparing here is what lets a populated
/// database upgrade without the manual prepare script.
async fn repair_partitioned(
    conn: &mut PgConnection,
    spec: &ManagedIndex,
    child_prefix: &str,
) -> Result<RepairReport, ManagedIndexError> {
    let table_exists: Option<String> =
        sqlx::query_scalar("SELECT to_regclass(format('%I.%I', current_schema(), $1))::text")
            .bind(spec.table)
            .fetch_one(&mut *conn)
            .await?;
    if table_exists.is_none() {
        // Fresh database: the migration creates table and index together.
        return Ok(RepairReport {
            index: spec.name,
            action: RepairAction::Untouched(IndexState::Absent),
            dropped_leftovers: Vec::new(),
        });
    }
    let dropped_leftovers = drop_rebuild_leftovers(conn, spec).await?;

    let mut built = 0;
    let mut reindexed = 0;
    let mut attached = 0;

    match inspect(conn, spec).await? {
        IndexState::WrongDefinition { expected, actual } => {
            return Err(ManagedIndexError::WrongDefinition {
                name: spec.name,
                table: spec.table,
                expected,
                actual,
            });
        }
        IndexState::Absent => {
            info!(
                index = spec.name,
                "creating partitioned parent index (metadata only)"
            );
            sqlx::query(&format!(
                "CREATE INDEX IF NOT EXISTS {} ON ONLY {} {}",
                ident(spec.name),
                ident(spec.table),
                spec.definition
            ))
            .execute(&mut *conn)
            .await?;
        }
        IndexState::Valid | IndexState::Invalid => {}
    }

    let children: Vec<PartitionChild> = sqlx::query_as(
        r#"
        SELECT heap.inhrelid AS partition_oid,
               part.relname AS partition,
               child.relname AS attached,
               ci.indisvalid AND ci.indisready AS attached_valid
        FROM pg_inherits heap
        JOIN pg_class part ON part.oid = heap.inhrelid
        LEFT JOIN pg_inherits att
               ON att.inhparent = to_regclass(format('%I.%I', current_schema(), $1))
        LEFT JOIN pg_index ci ON ci.indexrelid = att.inhrelid AND ci.indrelid = heap.inhrelid
        LEFT JOIN pg_class child ON child.oid = ci.indexrelid
        WHERE heap.inhparent = to_regclass(format('%I.%I', current_schema(), $2))
          AND (ci.indexrelid IS NOT NULL OR NOT EXISTS (
                SELECT 1 FROM pg_inherits a2 JOIN pg_index c2 ON c2.indexrelid = a2.inhrelid
                WHERE a2.inhparent = to_regclass(format('%I.%I', current_schema(), $1)) AND c2.indrelid = heap.inhrelid))
        ORDER BY heap.inhrelid
        "#,
    )
    .bind(spec.name)
    .bind(spec.table)
    .fetch_all(&mut *conn)
    .await?;

    for child in children {
        match (child.attached.as_deref(), child.attached_valid) {
            (Some(_), Some(true)) => continue,
            (Some(name), _) => {
                reindex_concurrently(conn, name).await?;
                reindexed += 1;
                continue;
            }
            (None, _) => {}
        }
        // No attached child: adopt a valid equivalent, rebuild an invalid one
        // with our name, or build a new one.
        let candidate_name = format!("{child_prefix}{}", child.partition_oid.0);
        let existing: Vec<(String, bool, String)> = sqlx::query_as(
            r#"
            SELECT c.relname, i.indisvalid AND i.indisready, pg_get_indexdef(i.indexrelid)
            FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
            WHERE i.indrelid = $1::oid
              AND NOT EXISTS (SELECT 1 FROM pg_inherits a WHERE a.inhrelid = i.indexrelid)
            ORDER BY c.relname
            "#,
        )
        .bind(child.partition_oid)
        .fetch_all(&mut *conn)
        .await?;
        let equivalent = |def: &str| definition_tail(def).as_deref() == Some(spec.definition);
        let mut to_attach: Option<String> = existing
            .iter()
            .find(|(_, valid, def)| *valid && equivalent(def))
            .map(|(name, _, _)| name.clone());
        if to_attach.is_none() {
            if let Some((name, _, _)) = existing
                .iter()
                .find(|(name, valid, def)| !*valid && *name == candidate_name && equivalent(def))
            {
                reindex_concurrently(conn, name).await?;
                reindexed += 1;
                to_attach = Some(name.clone());
            } else if let Some((name, _, def)) =
                existing.iter().find(|(name, _, _)| *name == candidate_name)
            {
                return Err(ManagedIndexError::WrongDefinition {
                    name: spec.name,
                    table: spec.table,
                    expected: spec.definition.to_string(),
                    actual: format!(
                        "{} on partition {} ({name})",
                        definition_tail(def).unwrap_or_default(),
                        child.partition
                    ),
                });
            } else {
                info!(index = spec.name, partition = %child.partition, child = %candidate_name, "building partition child index concurrently");
                sqlx::query(&format!(
                    "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} {}",
                    ident(&candidate_name),
                    ident(&child.partition),
                    spec.definition
                ))
                .execute(&mut *conn)
                .await?;
                built += 1;
                to_attach = Some(candidate_name.clone());
            }
        }
        if let Some(name) = to_attach {
            info!(index = spec.name, partition = %child.partition, child = %name, "attaching partition child index");
            sqlx::query(&format!(
                "ALTER INDEX {} ATTACH PARTITION {}",
                ident(spec.name),
                ident(&name)
            ))
            .execute(&mut *conn)
            .await?;
            attached += 1;
        }
    }

    let touched = built + reindexed + attached > 0;
    if touched {
        analyze(conn, spec.table).await?;
    }
    match inspect(conn, spec).await? {
        IndexState::Valid => {}
        state => {
            return Err(ManagedIndexError::StillUnusable {
                name: spec.name,
                table: spec.table,
                state,
            });
        }
    }
    let incomplete: Option<String> = sqlx::query_scalar(
        r#"
        SELECT part.relname
        FROM pg_inherits heap
        JOIN pg_class part ON part.oid = heap.inhrelid
        WHERE heap.inhparent = to_regclass(format('%I.%I', current_schema(), $2))
          AND NOT EXISTS (
              SELECT 1 FROM pg_inherits att
              JOIN pg_index ci ON ci.indexrelid = att.inhrelid
              WHERE att.inhparent = to_regclass(format('%I.%I', current_schema(), $1))
                AND ci.indrelid = heap.inhrelid AND ci.indisvalid AND ci.indisready)
        ORDER BY part.relname LIMIT 1
        "#,
    )
    .bind(spec.name)
    .bind(spec.table)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(partition) = incomplete {
        return Err(ManagedIndexError::PartitionIncomplete {
            name: spec.name,
            table: spec.table,
            partition,
        });
    }
    Ok(RepairReport {
        index: spec.name,
        action: if touched {
            RepairAction::PartitionsRepaired {
                built,
                reindexed,
                attached,
            }
        } else {
            RepairAction::Untouched(IndexState::Valid)
        },
        dropped_leftovers,
    })
}

/// Repair every managed index that needs it. Run before the migrator, on a
/// direct connection (concurrent builds cannot run through a transaction
/// pooler), with `applied` being the migration versions already recorded.
pub async fn repair_all(
    pool: &PgPool,
    specs: &[ManagedIndex],
    applied: &HashSet<i64>,
) -> Result<Vec<RepairReport>, ManagedIndexError> {
    let mut conn = pool.acquire().await?;
    prepare_session(&mut conn).await?;
    let mut reports = Vec::with_capacity(specs.len());
    for spec in specs {
        let report = match spec.kind {
            IndexKind::Plain => repair_plain(&mut conn, spec, applied).await?,
            IndexKind::Partitioned { child_prefix } => {
                repair_partitioned(&mut conn, spec, child_prefix).await?
            }
        };
        match &report.action {
            RepairAction::Untouched(state) => {
                tracing::debug!(index = spec.name, ?state, "managed index needs no repair")
            }
            action => info!(index = spec.name, ?action, "managed index repaired"),
        }
        reports.push(report);
    }
    Ok(reports)
}

/// After migrations: every managed index whose creating migration is recorded
/// must be valid. Absent-but-pending is fine (an older binary against a newer
/// database never reaches here; a newer binary has just applied it).
pub async fn verify_all(
    pool: &PgPool,
    specs: &[ManagedIndex],
    applied: &HashSet<i64>,
) -> Result<(), ManagedIndexError> {
    let mut conn = pool.acquire().await?;
    for spec in specs {
        if !applied.contains(&spec.created_by_migration) {
            continue;
        }
        match inspect(&mut conn, spec).await? {
            IndexState::Valid => {}
            IndexState::Absent => {
                return Err(ManagedIndexError::MissingAfterMigration {
                    name: spec.name,
                    table: spec.table,
                    version: spec.created_by_migration,
                });
            }
            IndexState::WrongDefinition { expected, actual } => {
                return Err(ManagedIndexError::WrongDefinition {
                    name: spec.name,
                    table: spec.table,
                    expected,
                    actual,
                });
            }
            state => {
                return Err(ManagedIndexError::StillUnusable {
                    name: spec.name,
                    table: spec.table,
                    state,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_deparsed_definitions() {
        let def = "CREATE INDEX idx_batches_owner_created_at_id ON fusillade.batches USING btree (created_by, created_at DESC, id DESC) WHERE (deleted_at IS NULL)";
        let (unique, name, table, tail) = split_indexdef(def).unwrap();
        assert!(!unique);
        assert_eq!(name, "idx_batches_owner_created_at_id");
        assert_eq!(table, "batches");
        assert_eq!(
            tail,
            "btree (created_by, created_at DESC, id DESC) WHERE (deleted_at IS NULL)"
        );
        assert_eq!(
            definition_tail(def).unwrap(),
            "USING btree (created_by, created_at DESC, id DESC) WHERE (deleted_at IS NULL)"
        );
        let unique_def = "CREATE UNIQUE INDEX u ON t USING btree (id)";
        assert_eq!(definition_tail(unique_def).unwrap(), "UNIQUE btree (id)");
    }

    #[test]
    fn classifies_catalog_rows() {
        let spec = &managed_indexes()[3];
        assert_eq!(classify(spec, None), IndexState::Absent);
        let row = CatalogIndex {
            indisvalid: false,
            indisready: true,
            relkind: "i".into(),
            indexdef: format!("CREATE INDEX {} ON batches {}", spec.name, spec.definition),
            heap_name: "batches".into(),
        };
        assert_eq!(classify(spec, Some(&row)), IndexState::Invalid);
        let valid = CatalogIndex {
            indisvalid: true,
            ..row
        };
        assert_eq!(classify(spec, Some(&valid)), IndexState::Valid);
        let wrong = CatalogIndex {
            indexdef: format!(
                "CREATE INDEX {} ON batches USING btree (created_by, created_at, id) WHERE (deleted_at IS NULL)",
                spec.name
            ),
            ..valid
        };
        assert!(matches!(
            classify(spec, Some(&wrong)),
            IndexState::WrongDefinition { .. }
        ));
    }

    /// Every registry entry must deparse exactly as written, otherwise repair
    /// would either rebuild with a different definition or reject a correct
    /// index as "wrong definition".
    #[sqlx::test]
    async fn managed_index_definitions_match_catalog(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        for spec in managed_indexes() {
            let row = catalog_index(&mut conn, spec.name)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{} should exist after migrations", spec.name));
            assert_eq!(
                definition_tail(&row.indexdef).as_deref(),
                Some(spec.definition),
                "registry definition for {} drifted from the catalog ({})",
                spec.name,
                row.indexdef
            );
            assert_eq!(
                inspect(&mut conn, spec).await.unwrap(),
                IndexState::Valid,
                "{}",
                spec.name
            );
        }
    }

    async fn applied_versions(pool: &PgPool) -> HashSet<i64> {
        sqlx::query_scalar::<_, i64>("SELECT version FROM _sqlx_migrations")
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .collect()
    }

    /// The 11.9.1 shape: creation migration recorded, index INVALID.
    #[sqlx::test]
    async fn invalid_plain_index_is_reindexed(pool: PgPool) {
        let spec = managed_indexes()
            .iter()
            .find(|s| s.name == "idx_batches_owner_created_at_id")
            .unwrap();
        sqlx::query("UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'idx_batches_owner_created_at_id'::regclass")
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(inspect(&mut conn, spec).await.unwrap(), IndexState::Invalid);
        drop(conn);

        let reports = repair_all(
            &pool,
            std::slice::from_ref(spec),
            &applied_versions(&pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reports[0].action, RepairAction::Reindexed);
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(inspect(&mut conn, spec).await.unwrap(), IndexState::Valid);
        // The validation migration's own check must now pass too.
        sqlx::raw_sql(include_str!(
            "../migrations/20260910010001_validate_batch_owner_page_index.up.sql"
        ))
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn absent_plain_index_with_recorded_migration_is_rebuilt(pool: PgPool) {
        let spec = managed_indexes()
            .iter()
            .find(|s| s.name == "idx_batches_owner_created_at_id")
            .unwrap();
        sqlx::query("DROP INDEX idx_batches_owner_created_at_id")
            .execute(&pool)
            .await
            .unwrap();
        let reports = repair_all(
            &pool,
            std::slice::from_ref(spec),
            &applied_versions(&pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reports[0].action, RepairAction::Built);
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(inspect(&mut conn, spec).await.unwrap(), IndexState::Valid);
    }

    #[sqlx::test]
    async fn absent_plain_index_with_pending_migration_is_left_to_the_migrator(pool: PgPool) {
        let spec = managed_indexes()
            .iter()
            .find(|s| s.name == "idx_batches_owner_created_at_id")
            .unwrap();
        sqlx::query("DROP INDEX idx_batches_owner_created_at_id")
            .execute(&pool)
            .await
            .unwrap();
        let reports = repair_all(&pool, std::slice::from_ref(spec), &HashSet::new())
            .await
            .unwrap();
        assert_eq!(
            reports[0].action,
            RepairAction::Untouched(IndexState::Absent)
        );
    }

    #[sqlx::test]
    async fn wrong_definition_fails_without_touching_the_index(pool: PgPool) {
        let spec = managed_indexes()
            .iter()
            .find(|s| s.name == "idx_batches_owner_created_at_id")
            .unwrap();
        sqlx::query("DROP INDEX idx_batches_owner_created_at_id")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX idx_batches_owner_created_at_id ON batches (created_by, created_at, id) WHERE deleted_at IS NULL")
            .execute(&pool)
            .await
            .unwrap();
        // Invalid *and* wrong: still never dropped.
        sqlx::query("UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'idx_batches_owner_created_at_id'::regclass")
            .execute(&pool)
            .await
            .unwrap();
        let err = repair_all(
            &pool,
            std::slice::from_ref(spec),
            &applied_versions(&pool).await,
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("wrong definition"), "{message}");
        assert!(message.contains("created_at DESC"), "{message}");
        assert!(message.contains("was not modified"), "{message}");
        let still_there: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_index WHERE indexrelid = 'idx_batches_owner_created_at_id'::regclass AND NOT indisvalid)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(still_there, "a wrongly defined index must never be dropped");
        assert!(
            verify_all(
                &pool,
                std::slice::from_ref(spec),
                &applied_versions(&pool).await
            )
            .await
            .is_err()
        );
    }

    #[sqlx::test]
    async fn rebuild_leftovers_are_dropped_before_reindex(pool: PgPool) {
        let spec = managed_indexes()
            .iter()
            .find(|s| s.name == "idx_batches_owner_created_at_id")
            .unwrap();
        // Simulate what an interrupted REINDEX CONCURRENTLY leaves behind.
        sqlx::query("CREATE INDEX idx_batches_owner_created_at_id_ccnew ON batches (created_by, created_at DESC, id DESC) WHERE deleted_at IS NULL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE pg_index SET indisvalid = false WHERE indexrelid IN ('idx_batches_owner_created_at_id_ccnew'::regclass, 'idx_batches_owner_created_at_id'::regclass)")
            .execute(&pool)
            .await
            .unwrap();
        let reports = repair_all(
            &pool,
            std::slice::from_ref(spec),
            &applied_versions(&pool).await,
        )
        .await
        .unwrap();
        assert_eq!(
            reports[0].dropped_leftovers,
            vec!["idx_batches_owner_created_at_id_ccnew".to_string()]
        );
        assert_eq!(reports[0].action, RepairAction::Reindexed);
        let leftover_exists: bool = sqlx::query_scalar(
            "SELECT to_regclass('idx_batches_owner_created_at_id_ccnew') IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!leftover_exists);
    }

    /// A valid unrelated index that merely shares the `_ccnew` suffix style but
    /// is valid is not a leftover and is kept.
    #[sqlx::test]
    async fn valid_lookalikes_are_not_dropped(pool: PgPool) {
        let spec = managed_indexes()
            .iter()
            .find(|s| s.name == "idx_batches_owner_created_at_id")
            .unwrap();
        sqlx::query("CREATE INDEX idx_batches_owner_created_at_id_ccnew ON batches (id)")
            .execute(&pool)
            .await
            .unwrap();
        let reports = repair_all(
            &pool,
            std::slice::from_ref(spec),
            &applied_versions(&pool).await,
        )
        .await
        .unwrap();
        assert!(reports[0].dropped_leftovers.is_empty());
        sqlx::query("DROP INDEX idx_batches_owner_created_at_id_ccnew")
            .execute(&pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn partitioned_index_is_completed_for_populated_partitions(pool: PgPool) {
        let spec = managed_indexes()
            .iter()
            .find(|s| s.name == "idx_retained_response_objects_created")
            .unwrap();
        // Two populated partitions, then take the managed index away entirely
        // (the shape of a populated 11.8 database about to receive the index).
        sqlx::query("SELECT ensure_retained_response_partition(DATE '2031-01-01')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT ensure_retained_response_partition(DATE '2031-01-02')")
            .execute(&pool)
            .await
            .unwrap();
        for day in ["2031-01-01", "2031-01-02"] {
            sqlx::query(&format!(
                "INSERT INTO retained_response_objects (delete_on, group_id, object_kind, object_id, created_at, schema_version, payload)
                 VALUES (DATE '{day}', gen_random_uuid(), 'request', gen_random_uuid(), now(), 1, '{{}}'::jsonb)"
            ))
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("DROP INDEX idx_retained_response_objects_created")
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(inspect(&mut conn, spec).await.unwrap(), IndexState::Absent);
        drop(conn);

        let reports = repair_all(&pool, std::slice::from_ref(spec), &HashSet::new())
            .await
            .unwrap();
        match reports[0].action {
            RepairAction::PartitionsRepaired {
                built, attached, ..
            } => {
                assert_eq!(built, 2);
                assert_eq!(attached, 2);
            }
            ref other => panic!("unexpected action {other:?}"),
        }
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(inspect(&mut conn, spec).await.unwrap(), IndexState::Valid);
        // The migration's own validation passes on the prepared database.
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20260910000000_add_retained_response_created_index.up.sql"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // Second pass is a no-op.
        let reports = repair_all(&pool, std::slice::from_ref(spec), &HashSet::new())
            .await
            .unwrap();
        assert_eq!(
            reports[0].action,
            RepairAction::Untouched(IndexState::Valid)
        );
    }

    /// Partition added after the parent index existed but whose child build was
    /// interrupted: the invalid child is rebuilt and attached.
    #[sqlx::test]
    async fn invalid_partition_child_is_reindexed_and_attached(pool: PgPool) {
        let spec = managed_indexes()
            .iter()
            .find(|s| s.name == "idx_retained_response_objects_created")
            .unwrap();
        sqlx::query("SELECT ensure_retained_response_partition(DATE '2031-02-01')")
            .execute(&pool)
            .await
            .unwrap();
        // Detach the inherited child and mark it invalid, as an interrupted
        // concurrent child build would look.
        let child: String = sqlx::query_scalar(
            "SELECT c.relname FROM pg_inherits a JOIN pg_class c ON c.oid = a.inhrelid JOIN pg_index i ON i.indexrelid = c.oid
             WHERE a.inhparent = 'idx_retained_response_objects_created'::regclass AND i.indrelid = 'retained_response_objects_d20310201'::regclass",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(&format!(
            "UPDATE pg_index SET indisvalid = false WHERE indexrelid = '{child}'::regclass"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let reports = repair_all(&pool, std::slice::from_ref(spec), &HashSet::new())
            .await
            .unwrap();
        assert_eq!(
            reports[0].action,
            RepairAction::PartitionsRepaired {
                built: 0,
                reindexed: 1,
                attached: 0
            }
        );
        assert!(
            verify_all(
                &pool,
                std::slice::from_ref(spec),
                &HashSet::from([spec.created_by_migration])
            )
            .await
            .is_ok()
        );
    }
}
