-- no-transaction
-- Reverting returns late-finish queries to scanning all of batches.
DROP INDEX CONCURRENTLY IF EXISTS idx_batches_finished_late;
