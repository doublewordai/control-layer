-- no-transaction
--
-- Drop the finalizer sweep index (COR-649). Reverting returns all three
-- finalizer arms to seq-scanning `batches` on every tick.
--
-- CONCURRENTLY for the same reason as the up migration, which requires the
-- `-- no-transaction` directive above and one statement in the file.

DROP INDEX CONCURRENTLY IF EXISTS idx_batches_unfrozen_sweep;
