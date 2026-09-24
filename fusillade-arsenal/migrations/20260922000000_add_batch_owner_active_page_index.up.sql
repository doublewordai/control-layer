-- no-transaction
--
-- Bound owner-scoped active-first batch pages to the owner's live
-- backlog.
--
-- The active-first list arm is
--
--   SELECT b.* FROM batches b
--   WHERE b.deleted_at IS NULL AND (b.completed_at IS NULL AND b.failed_at
--     IS NULL AND b.cancelled_at IS NULL AND b.cancelling_at IS NULL)
--     AND b.created_by = $owner
--   ORDER BY b.created_at DESC, b.id DESC LIMIT $page
--
-- (see fusillade-arsenal/src/postgres/batch_list.rs). Before this index, the
-- only ordered candidate for that shape was idx_batches_owner_created_at_id,
-- which walks the owner's whole history newest-first and filters active rows
-- only after reading them, or idx_batches_active, which is ordered by id and
-- needs a full scan + sort. Owners with large terminal histories paid the
-- former on every active-first page; platform-wide views and growing live
-- backlogs paid the latter. The 2026-09-22 /ai/v1/batches 5xx alert traced to
-- exactly this walk: a customer fan-out kept the global active set growing
-- through the afternoon until pages tripped the 15s page budget.
--
-- This index holds only active rows keyed (created_by, created_at DESC,
-- id DESC), so the arm above walks the owner's active rows in page order and
-- stops at the limit, under generic prepared plans too: the predicate
-- references no bind parameters, so it survives sqlx's cached plans. The
-- unscoped (platform-wide) arm is served by idx_batches_active_first_sort,
-- which already orders by (active, created_at DESC, id DESC).
--
-- The predicate matches the ACTIVE constant in batch_list.rs arm-for-arm
-- (with the b. alias dropped). If the active definition ever changes, this
-- index must change with it — the validation migration that follows pins the
-- predicate text.
--
-- Built CONCURRENTLY per docs/migrations.md ("Concurrent index builds"): the
-- `-- no-transaction` directive above must stay the first bytes of the file
-- and the file may hold exactly one statement, because CONCURRENTLY cannot
-- run inside the implicit transaction Postgres wraps around a multi-statement
-- simple query. The reindex and validate migrations that follow make an
-- interrupted build recoverable and verify the definition. Same prebuild
-- guidance as 20260910010000: for populated deployments, run this file on a
-- direct connection first if the build would compete with peak traffic, then
-- let the migration Job record it.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_owner_active_created_at_id
ON batches (created_by, created_at DESC, id DESC)
WHERE deleted_at IS NULL
  AND completed_at IS NULL
  AND failed_at IS NULL
  AND cancelled_at IS NULL
  AND cancelling_at IS NULL;