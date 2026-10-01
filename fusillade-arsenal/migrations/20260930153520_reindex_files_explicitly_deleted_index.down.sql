-- no-transaction
-- An interrupted REINDEX INDEX CONCURRENTLY can leave an invalid
-- idx_files_explicitly_deleted_ccnew behind. Dropping it here, before
-- 20260930153510's down migration drops the index itself, leaves nothing
-- behind. DROP INDEX CONCURRENTLY accepts one name per statement.
DROP INDEX CONCURRENTLY IF EXISTS idx_files_explicitly_deleted_ccnew;
