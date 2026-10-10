-- no-transaction
-- An interrupted REINDEX INDEX CONCURRENTLY can leave an invalid
-- idx_requests_tolerated_completions_ccnew behind. Dropping it here, before
-- 20261008120010's down migration drops the index itself, leaves nothing
-- behind. DROP INDEX CONCURRENTLY accepts one name per statement.
DROP INDEX CONCURRENTLY IF EXISTS idx_requests_tolerated_completions_ccnew;
