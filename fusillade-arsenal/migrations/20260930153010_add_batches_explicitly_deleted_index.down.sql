-- no-transaction
-- Reverting returns the batch purges to scanning all of batches.
DROP INDEX CONCURRENTLY IF EXISTS idx_batches_explicitly_deleted;
