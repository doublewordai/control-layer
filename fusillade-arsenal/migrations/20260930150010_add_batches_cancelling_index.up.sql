-- no-transaction
-- Candidate lookup for the daemon's cancellation poll (get_cancelled_batch_ids).
--
-- Each poll sends every batch id the daemon currently has in-flight work for,
-- which reaches hundreds of thousands of ids when many small batches are in
-- flight, and asks which of them are being cancelled. With only batches_pkey
-- available, every id costs a heap fetch of `batches` just to test
-- cancelling_at, so the poll reads a large share of the table each time.
-- This index holds only cancelling, undeleted batches, so the same
-- `id = ANY($1)` probe is answered from a small index and touches the heap
-- only for batches that actually match.
--
-- The predicate is exactly the poll's static conditions, so a generic
-- prepared plan can prove it. If that query's WHERE changes, change this
-- predicate with it.
--
-- Keep this file to one statement: concurrent builds cannot run in a
-- transaction. A failed build can leave an INVALID index that IF NOT EXISTS
-- skips; the following reindex and validate migrations repair and check it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_cancelling
ON batches (id)
WHERE cancelling_at IS NOT NULL AND deleted_at IS NULL;
