-- Post-deploy work for migration 20260507000000. Two things live here that
-- can't go inside the migration itself:
--
--   A. CREATE INDEX CONCURRENTLY for the two partial indexes that back the
--      per-user listing query. CONCURRENTLY can't run in a transaction, and
--      sqlx wraps every migration in one, so the index DDL lives here and
--      runs in autocommit. Each statement is its own implicit transaction.
--   B. A one-shot data conversion for pre-existing realtime/flex responses
--      stored as single-row "virtual" batches (a `files` row with
--      name='single_request' and purpose='batch', a `batches` row, and a
--      `requests` row). The new code path inserts those responses directly
--      with `requests.created_by` set and `batch_id` NULL; this script
--      converts the old shape so old rows appear in the new listing.
--
-- Run with psql (NOT inside `BEGIN; ... COMMIT;`) once after deploying the
-- migration. Both sections are idempotent:
--   - CREATE INDEX uses IF NOT EXISTS.
--   - The data conversion's WHERE clauses match nothing on a second pass.
--
-- =========================================================================
-- A. Per-user listing indexes (CONCURRENTLY, outside any transaction).
-- =========================================================================
-- Two partial indexes covering the active-first and recency sort orderings
-- of list_requests, scoped to batchless rows. Batched rows (created_by IS
-- NULL) still go through the existing batch-driven path and are excluded.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_user_active_sort
ON requests (
    created_by,
    (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
    created_at DESC,
    id DESC,
    service_tier
) WHERE created_by IS NOT NULL;

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_user_created_sort
ON requests (
    created_by,
    created_at DESC,
    id DESC,
    service_tier
) WHERE created_by IS NOT NULL;

-- =========================================================================
-- B. Virtual-batch -> batchless response data conversion.
-- =========================================================================
-- Behaviour:
--   1. Hard-delete request rows whose virtual batch was soft-deleted
--      (`b.deleted_at IS NOT NULL`). The pre-migration listing filtered
--      these out via `b.deleted_at IS NULL`, so dropping the row preserves
--      the tombstone.
--   2. For live virtual batches, copy `b.created_by` onto the request and
--      null out `batch_id`. The row then satisfies the new listing's
--      `r.created_by IS NOT NULL` predicate (and the XOR CHECK that
--      requires exactly one of batch_id/created_by to be set).
--   3. Virtual `batches` and `files` rows are intentionally left in place;
--      they're orphan rows now (no children) but harmless. A separate
--      cleanup pass can prune them later.
--
-- Distinguishing virtual batches from genuine user-uploaded ones:
-- `files.name = 'single_request'` alone is user-collidable (file names come
-- from clients). The legacy `create_single_request_batch` path also leaves
-- `files.uploaded_by` NULL (the upload API always sets it) and produces
-- batches with exactly `total_requests = 1`. Requiring all three together
-- makes a real user batch effectively impossible to match by accident.

BEGIN;

-- 1. Tombstoned responses: hard-delete.
DELETE FROM requests
WHERE batch_id IN (
    SELECT b.id FROM batches b
    JOIN files f ON b.file_id = f.id
    WHERE f.name = 'single_request'
      AND f.purpose = 'batch'
      AND f.uploaded_by IS NULL
      AND b.total_requests = 1
      AND b.deleted_at IS NOT NULL
);

-- 2. Live virtual-batch responses: copy attribution and detach from the batch.
UPDATE requests r
SET created_by = b.created_by, batch_id = NULL
FROM batches b
JOIN files f ON b.file_id = f.id
WHERE r.batch_id = b.id
  AND f.name = 'single_request'
  AND f.purpose = 'batch'
  AND f.uploaded_by IS NULL
  AND b.total_requests = 1
  AND b.deleted_at IS NULL;

COMMIT;

-- =========================================================================
-- C. Refresh table statistics so the planner uses the new partial indexes.
-- =========================================================================
-- The migration's `ADD COLUMN created_by` left the planner without a
-- histogram for the new column, and section B above just mass-updated
-- existing rows to populate it. Without an ANALYZE, the planner estimates
-- predicates against `created_by` return ~1 row, which silently picks the
-- wrong index for the per-user listing query (observed 1200ms vs 0.3ms
-- after ANALYZE on a 285k-row staging dataset).
--
-- ANALYZE only samples (default 30k rows) and takes SHARE UPDATE EXCLUSIVE
-- — doesn't block reads/writes. Runs outside a transaction (the BEGIN/COMMIT
-- above closed) so stats become visible immediately on completion.
ANALYZE requests;
