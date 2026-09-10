-- no-transaction
-- Roll back the query first. This only removes COR-537's additional index.
DROP INDEX CONCURRENTLY IF EXISTS idx_batches_owner_created_at_id;
