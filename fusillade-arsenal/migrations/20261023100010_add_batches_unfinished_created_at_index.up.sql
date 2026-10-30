-- no-transaction
--
-- Driving set for the retirement guards on both content families. A weekly
-- partition of batch_requests_archive or request_templates_g2 drops once it
-- is past its retention period and no unfinished batch can still write into
-- or read from it:
--
--   WHERE counts_frozen_at IS NULL AND deleted_at IS NULL
--     AND created_at >= <week start> AND created_at < <week end>
--
-- Almost every batch is frozen within a day, so the predicate keeps the
-- index to the few dozen batches still running, and the guard reads those
-- rather than scanning all of batches once per retirement tick.
--
-- One statement only: CONCURRENTLY cannot run in a transaction. The following
-- migrations reindex (repairing an interrupted build) and validate it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_unfinished_created_at
ON batches (created_at)
WHERE counts_frozen_at IS NULL
  AND deleted_at IS NULL;
