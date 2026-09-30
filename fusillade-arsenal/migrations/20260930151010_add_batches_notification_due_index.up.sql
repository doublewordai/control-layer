-- no-transaction
--
-- Candidate lookup for claim_batch_notifications.
--
-- The notification claim selects frozen, unnotified batches oldest-frozen
-- first:
--
--   WHERE counts_frozen_at IS NOT NULL AND notification_sent_at IS NULL
--     AND cancelling_at IS NULL AND deleted_at IS NULL AND total_requests > 0
--   ORDER BY counts_frozen_at LIMIT 100 FOR UPDATE SKIP LOCKED
--
-- Without this index the planner satisfies the ORDER BY from
-- idx_batches_retention_due, which holds every frozen batch, and filters
-- notification_sent_at row by row through the heap. Almost every frozen batch
-- is already notified, so each claim walks the whole frozen history to reach
-- the few pending rows; under load the claim outlives its timeout and
-- notifications stop going out.
--
-- The predicate is exactly the claim's static conditions, so it stays implied
-- under generic plans, and the key supplies the ORDER BY. A row enters when a
-- batch is frozen and leaves when its notification is claimed, so the index
-- tracks the pending notifications rather than history.
--
-- One statement only: CONCURRENTLY cannot run in a transaction. The following
-- migrations reindex (repairing an interrupted build) and validate it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_notification_due
ON batches (counts_frozen_at)
WHERE counts_frozen_at IS NOT NULL
  AND notification_sent_at IS NULL
  AND cancelling_at IS NULL
  AND deleted_at IS NULL
  AND total_requests > 0;
