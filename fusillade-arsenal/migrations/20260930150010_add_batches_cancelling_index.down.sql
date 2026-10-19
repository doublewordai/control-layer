-- no-transaction
-- Revert the cancellation poll to batches_pkey heap probes.
DROP INDEX CONCURRENTLY IF EXISTS idx_batches_cancelling;
