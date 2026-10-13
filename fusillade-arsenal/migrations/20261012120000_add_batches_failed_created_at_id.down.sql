-- no-transaction
-- Revert failed-batch pages to scanning idx_batches_created_at.
DROP INDEX CONCURRENTLY IF EXISTS idx_batches_failed_created_at_id;
