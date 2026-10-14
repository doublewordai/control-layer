-- no-transaction
-- An interrupted REINDEX INDEX CONCURRENTLY can leave an invalid
-- idx_batches_finished_late_ccnew behind. Dropping it here, before
-- 20261013100010's down migration drops the index itself, leaves nothing
-- behind. DROP INDEX CONCURRENTLY accepts one name per statement.
DROP INDEX CONCURRENTLY IF EXISTS idx_batches_finished_late_ccnew;
