-- no-transaction
-- Rebuild idx_batches_cancelling concurrently. On a clean build this is one
-- extra pass without a write lock; after an interrupted build it turns the
-- INVALID index left behind into a valid one. Neither step is recorded until
-- it succeeds, so a retry resumes here. One statement only (no transaction).
REINDEX INDEX CONCURRENTLY idx_batches_cancelling;
