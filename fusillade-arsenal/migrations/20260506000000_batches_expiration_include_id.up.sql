-- Add an id-covering version of idx_batches_active_by_expiration so the
-- pending-request-counts query (and any other expires_at-driven join on
-- batch id) can satisfy the join key from the index alone, avoiding ~500K
-- heap fetches per call in production.
--
-- Production deploy: build the new index CONCURRENTLY first so this migration
-- becomes a no-op on the CREATE side. The DROP of the old index is fine in
-- a regular transaction (brief ACCESS EXCLUSIVE lock):
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_active_expiration_with_id
--     ON batches (expires_at)
--     INCLUDE (id)
--     WHERE cancelling_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_batches_active_expiration_with_id
  ON batches (expires_at)
  INCLUDE (id)
  WHERE cancelling_at IS NULL;

COMMENT ON INDEX idx_batches_active_expiration_with_id IS
'Active-batch expiration index with id INCLUDEd to enable index-only scans for joins keyed on batch id';

DROP INDEX IF EXISTS idx_batches_active_by_expiration;
