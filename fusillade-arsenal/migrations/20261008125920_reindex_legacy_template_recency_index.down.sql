-- no-transaction
-- An interrupted concurrent reindex can leave this temporary index behind.
DROP INDEX CONCURRENTLY IF EXISTS idx_request_templates_retirement_created_at_ccnew;
