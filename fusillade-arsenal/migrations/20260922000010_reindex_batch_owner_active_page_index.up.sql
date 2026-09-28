-- no-transaction
--
-- Self-healing pass for the index built by 20260922000000
-- (docs/migrations.md, "Concurrent index builds"). On a clean build this is
-- one extra bounded concurrent pass with no write lock; on an interrupted
-- build it turns the INVALID leftover index into a valid one, and if it is
-- itself interrupted the next run repeats it. SQLx records the migration only
-- after the statement returns, so a retry always resumes here.
--
-- If a prior REINDEX was interrupted it may leave an invalid
-- idx_batches_owner_active_created_at_id_ccnew behind; this statement
-- tolerates that leftover. Removing it needs its own `-- no-transaction`
-- file: DROP INDEX CONCURRENTLY IF EXISTS idx_batches_owner_active_created_at_id_ccnew.

REINDEX INDEX CONCURRENTLY idx_batches_owner_active_created_at_id;