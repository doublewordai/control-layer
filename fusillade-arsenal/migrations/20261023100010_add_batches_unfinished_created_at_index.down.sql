-- no-transaction
-- Reverting returns the retirement guards to scanning all of batches.
DROP INDEX CONCURRENTLY IF EXISTS idx_batches_unfinished_created_at;
