//! Moving a frozen batch's request rows from `requests` into its weekly
//! `batch_requests_archive` partition.
//!
//! A batch moves in bounded chunks, one transaction per chunk. Each chunk
//! copies up to `chunk_rows` of the batch's live rows into the archive and
//! deletes exactly those rows from `requests` in the same transaction, so every
//! committed state keeps each row in exactly one table and a reader's
//! live-plus-archive union sees it once. Between chunks the batch is `split`
//! (some rows archived, the rest live), the same shape a retry leaves, which
//! every reader, retry and freeze path already handles; the chunk that moves
//! the last row stamps `archive`.
//!
//! A failed or abandoned chunk rolls back only its own rows; earlier chunks
//! stay committed and the next pass resumes from what remains, so no copy is
//! ever repeated. A batch that fits in one chunk moves in a single
//! transaction, as before. Each call works within the daemon's maintenance
//! deadline and leaves the rest of a large batch for the next pass.

use std::time::Instant;

use anyhow::anyhow;
use uuid::Uuid;

use super::ArchiveOutcome;
use crate::batch::BatchId;
use crate::error::{FusilladeError, Result};
use crate::{PoolProvider, PostgresRequestManager};

/// Rows moved per archive transaction. Bounds a transaction's work however
/// large the batch or its response bodies are.
pub(super) const ARCHIVE_MOVE_CHUNK_ROWS: i64 = 1_000;

enum ChunkOutcome {
    /// The chunk moved the batch's last live rows; the batch is `archive`.
    Finished { rows: u64 },
    /// The chunk committed and live rows remain; the batch is `split`.
    Moved { rows: u64 },
    /// Nothing moved; the batch is not a candidate right now.
    Skipped(ArchiveOutcome),
}

/// Move `batch_id` chunk by chunk within one maintenance call.
///
/// Every statement of every chunk ends by the call's maintenance deadline,
/// derived from the daemon's configured query timeout. New chunks start only
/// during the first half of the call's budget, so the last chunk started has
/// at least half the budget to finish instead of being cancelled part-way.
pub(super) async fn archive_batch_in_chunks<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    batch_id: BatchId,
    chunk_rows: i64,
) -> Result<ArchiveOutcome> {
    super::request_maintenance::before_archive(manager).await?;
    let started = Instant::now();
    let deadline = manager.maintenance_deadline();
    let last_start = started + deadline.saturating_duration_since(started) / 2;
    archive_batch_until(manager, batch_id, chunk_rows, last_start, deadline).await
}

/// Move chunks until the batch is fully archived, it stops being a
/// candidate, or `last_start` has passed; each chunk's statements end by
/// `deadline`. The first chunk always starts.
pub(super) async fn archive_batch_until<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    batch_id: BatchId,
    chunk_rows: i64,
    last_start: Instant,
    deadline: Instant,
) -> Result<ArchiveOutcome> {
    let mut moved = 0u64;
    loop {
        match move_chunk(manager, batch_id, chunk_rows, deadline).await? {
            ChunkOutcome::Finished { rows } => {
                let rows = moved + rows;
                tracing::info!(%batch_id, rows, "Archived batch rows");
                return Ok(ArchiveOutcome::Archived { rows });
            }
            ChunkOutcome::Moved { rows } => {
                moved += rows;
                if Instant::now() >= last_start {
                    tracing::info!(%batch_id, rows = moved, "Archived part of a batch; resuming next pass");
                    return Ok(ArchiveOutcome::Progressed { rows: moved });
                }
            }
            ChunkOutcome::Skipped(outcome) => {
                if moved > 0 {
                    tracing::info!(%batch_id, rows = moved, ?outcome, "Archived part of a batch before it stopped being a candidate");
                }
                return Ok(stopped_outcome(moved, outcome));
            }
        }
    }
}

/// The call's outcome when a chunk moved nothing. A batch that stops being a
/// candidate between chunks (a retry, a cancel, a concurrent mover) keeps what
/// already moved, reported as progress. A missing or fenced partition is
/// reported as such even after progress, so the daemon still raises it; the
/// batch stays `split` with every row in exactly one table.
fn stopped_outcome(moved: u64, outcome: ArchiveOutcome) -> ArchiveOutcome {
    match outcome {
        ArchiveOutcome::SkippedNoPartition => ArchiveOutcome::SkippedNoPartition,
        _ if moved > 0 => ArchiveOutcome::Progressed { rows: moved },
        outcome => outcome,
    }
}

/// Bound the next statement of a chunk transaction to the call's deadline.
async fn bound(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, deadline: Instant) -> Result<()> {
    super::bound_to_deadline(tx, deadline)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to bound archive move: {}", e)))
}

async fn move_chunk<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    batch_id: BatchId,
    chunk_rows: i64,
    deadline: Instant,
) -> Result<ChunkOutcome> {
    let mut tx = manager
        .begin_maintenance_write_until(deadline)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to begin transaction: {}", e)))?;

    // Lock the batch row for the whole chunk. Retry / cancel / freeze all
    // UPDATE this row, so they queue behind the chunk (and vice versa) — no
    // interleaving is possible while we hold the lock. The bucket is derived
    // HERE, in UTC (`AT TIME ZONE 'UTC'` so the ISO-week Monday can never
    // depend on the session TimeZone), and stamped by the first chunk; every
    // later chunk and reader uses the stamped value, never re-derives.
    //
    // SKIP LOCKED, not a plain FOR UPDATE: concurrent movers all walk the
    // same oldest-first candidate list, so with a waiting lock they would
    // serialize behind whichever mover holds the current oldest batch.
    // Bouncing off a held row and reporting SkippedNotLive (with the
    // contention counted via fusillade_archive_contended_total) lets each
    // mover fall through to its next candidate — disjoint work, no
    // coordinator.
    let batch = sqlx::query!(
        r#"
        SELECT retry_version,
               location,
               counts_frozen_at,
               COALESCE(
                   archive_bucket,
                   -- A batch that outlived its own week's retirement (it
                   -- was blocked from moving until after the week was
                   -- dropped) is routed into the current week instead:
                   -- a retired week never receives rows, and a later
                   -- retirement date is the safe direction.
                   CASE
                       WHEN EXISTS (
                           SELECT 1
                           FROM batch_archive_buckets bucket
                           WHERE bucket.week_start =
                                 date_trunc('week', created_at AT TIME ZONE 'UTC')::date
                             AND bucket.state <> 'active'
                       )
                       THEN date_trunc('week', now() AT TIME ZONE 'UTC')::date
                       ELSE date_trunc('week', created_at AT TIME ZONE 'UTC')::date
                   END
               ) AS "bucket!"
        FROM batches
        WHERE id = $1 AND deleted_at IS NULL
        FOR UPDATE SKIP LOCKED
        "#,
        *batch_id as Uuid,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to lock batch for archive: {}", e)))?;

    let Some(batch) = batch else {
        // No row can mean "missing/deleted" or "exists but locked by another
        // mover" — SKIP LOCKED conflates them. Disambiguate: contention is
        // routine under concurrent movers and is counted here (it
        // deliberately does NOT get its own public outcome — to the caller
        // the batch is simply not available, same as already-archived),
        // while a persisting NotFound for a listed candidate would be odd.
        bound(&mut tx, deadline).await?;
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM batches WHERE id = $1 AND deleted_at IS NULL) AS "exists!""#,
            *batch_id as Uuid,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to check batch existence: {}", e)))?;
        return Ok(ChunkOutcome::Skipped(if exists {
            metrics::counter!("fusillade_archive_contended_total").increment(1);
            ArchiveOutcome::SkippedNotLive
        } else {
            ArchiveOutcome::SkippedNotFound
        }));
    };
    if batch.location == "archive" {
        return Ok(ChunkOutcome::Skipped(ArchiveOutcome::SkippedNotLive));
    }
    if batch.counts_frozen_at.is_none() {
        return Ok(ChunkOutcome::Skipped(ArchiveOutcome::SkippedNotFrozen));
    }

    // Hold a share lock on the target week's registry row for the rest of the
    // chunk. Retirement fences a week by UPDATEing this row, so a fence either
    // waits for this chunk to commit or was already visible here; rows can
    // never land in a week after it was fenced. A fenced week also stops a
    // part-moved batch here: it stays `split`, and retirement will not drop a
    // week that still has a non-archived batch in it.
    bound(&mut tx, deadline).await?;
    let bucket_state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM batch_archive_buckets WHERE week_start = $1 FOR SHARE",
    )
    .bind(batch.bucket)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to lock archive bucket: {}", e)))?;
    if bucket_state.is_some_and(|state| state != "active") {
        return Ok(ChunkOutcome::Skipped(ArchiveOutcome::SkippedNoPartition));
    }

    // Graceful degradation: a missing partition means the batch stays where
    // it is — fully served — and the caller alerts. Name derivation must
    // match ensure_archive_partitions() exactly.
    bound(&mut tx, deadline).await?;
    let partition_exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_inherits inheritance
            JOIN pg_class parent ON parent.oid = inheritance.inhparent
            JOIN pg_class child ON child.oid = inheritance.inhrelid
            JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
            WHERE parent.oid = 'batch_requests_archive'::regclass
              AND namespace.nspname = current_schema()
              AND child.relname =
                  'batch_requests_archive_y' || to_char($1::date, 'IYYY')
                      || 'w' || to_char($1::date, 'IW')
              AND NOT inheritance.inhdetachpending
        ) AS "exists!"
        "#,
        batch.bucket,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to check archive partition: {}", e)))?;
    if !partition_exists {
        return Ok(ChunkOutcome::Skipped(ArchiveOutcome::SkippedNoPartition));
    }

    // The chunk: any `chunk_rows` of the batch's remaining live rows, found
    // through the batch_id index. No ORDER BY: every chunk deletes what it
    // copies, so the next chunk simply takes from what is left, and ordering
    // by id would invite a primary-key walk across the whole table.
    bound(&mut tx, deadline).await?;
    let chunk: Vec<Uuid> = sqlx::query_scalar!(
        r#"SELECT id FROM requests WHERE batch_id = $1 LIMIT $2"#,
        *batch_id as Uuid,
        chunk_rows,
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to select archive chunk: {}", e)))?;

    // Forward copy. Positional alignment (`r.*, $bucket`) is guaranteed by the
    // schema-parity test suite (archive = requests' columns + archive_bucket
    // appended last). A committed chunk never leaves a row in both tables, so
    // the conflict arm only guards against a row that somehow already exists
    // in the archive; it never fires on the normal path, which keeps the
    // insert free of speculative-insertion garbage.
    bound(&mut tx, deadline).await?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO batch_requests_archive
        SELECT r.*, $2::date
        FROM requests r
        WHERE r.batch_id = $1 AND r.id = ANY($3)
        ON CONFLICT (id, archive_bucket) DO NOTHING
        "#,
    )
    .bind(*batch_id as Uuid)
    .bind(batch.bucket)
    .bind(&chunk)
    .execute(&mut *tx)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to copy rows to archive: {}", e)))?
    .rows_affected();

    // Exactly-one-table invariant, enforced structurally per chunk: only
    // delete rows that verifiably exist in the archive, then prove none of the
    // chunk was left behind. A row can never be deleted un-copied, and a torn
    // chunk aborts its transaction instead of committing.
    bound(&mut tx, deadline).await?;
    let deleted = sqlx::query!(
        r#"
        DELETE FROM requests r
        WHERE r.batch_id = $1
          AND r.id = ANY($3)
          AND EXISTS (
              SELECT 1 FROM batch_requests_archive a
              WHERE a.id = r.id AND a.archive_bucket = $2
          )
        "#,
        *batch_id as Uuid,
        batch.bucket,
        &chunk,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to delete archived rows: {}", e)))?
    .rows_affected();
    if deleted != chunk.len() as u64 {
        // Rolls back via drop of `tx`.
        return Err(FusilladeError::Other(anyhow!(
            "archive move for batch {batch_id} would leave {} of {} chunk rows in live \
             (inserted {inserted}, deleted {deleted}); aborted to preserve the \
             exactly-one-table invariant",
            chunk.len() as u64 - deleted,
            chunk.len()
        )));
    }

    bound(&mut tx, deadline).await?;
    let rows_remain = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM requests WHERE batch_id = $1) AS "remain!""#,
        *batch_id as Uuid,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to check remaining rows: {}", e)))?;

    // Location stamp with retry_version CAS: `archive` once nothing is left
    // live, `split` while rows remain. The FOR UPDATE lock already excludes
    // racing writers on this path; the CAS is belt-and-braces for any future
    // caller that reaches this UPDATE via a weaker lock (EvalPlanQual
    // re-checks target-row conditions after lock waits).
    let target = if rows_remain { "split" } else { "archive" };
    bound(&mut tx, deadline).await?;
    let stamped = sqlx::query!(
        r#"
        UPDATE batches
        SET location = $4, archive_bucket = $2
        WHERE id = $1
          AND retry_version = $3
          AND counts_frozen_at IS NOT NULL
          AND location IN ('live', 'split')
        "#,
        *batch_id as Uuid,
        batch.bucket,
        batch.retry_version,
        target,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| FusilladeError::Other(anyhow!("Failed to stamp batch location: {}", e)))?
    .rows_affected();
    if stamped == 0 {
        return Ok(ChunkOutcome::Skipped(ArchiveOutcome::SkippedRetryRaced));
    }

    tx.commit()
        .await
        .map_err(|e| FusilladeError::Other(anyhow!("Failed to commit archive move: {}", e)))?;

    Ok(if rows_remain {
        ChunkOutcome::Moved { rows: deleted }
    } else {
        ChunkOutcome::Finished { rows: deleted }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_partition_is_reported_even_after_progress() {
        assert_eq!(
            stopped_outcome(4, ArchiveOutcome::SkippedNoPartition),
            ArchiveOutcome::SkippedNoPartition
        );
        assert_eq!(
            stopped_outcome(0, ArchiveOutcome::SkippedNoPartition),
            ArchiveOutcome::SkippedNoPartition
        );
    }

    #[test]
    fn other_stops_after_progress_report_the_progress() {
        for outcome in [
            ArchiveOutcome::SkippedNotLive,
            ArchiveOutcome::SkippedNotFrozen,
            ArchiveOutcome::SkippedRetryRaced,
            ArchiveOutcome::SkippedNotFound,
        ] {
            assert_eq!(
                stopped_outcome(4, outcome),
                ArchiveOutcome::Progressed { rows: 4 }
            );
            assert_eq!(stopped_outcome(0, outcome), outcome);
        }
    }
}
