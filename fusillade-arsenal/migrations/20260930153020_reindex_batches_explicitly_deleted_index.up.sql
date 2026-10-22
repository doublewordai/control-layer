-- no-transaction
-- Rebuild the index created by 20260930153010. On a clean build this is one
-- extra concurrent pass; after an interrupted build it turns the INVALID index
-- left behind into a valid one. See docs/migrations.md, "Concurrent index
-- builds".
REINDEX INDEX CONCURRENTLY idx_batches_explicitly_deleted;
