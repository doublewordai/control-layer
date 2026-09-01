DROP INDEX IF EXISTS idx_batches_active_expiration_with_id;

CREATE INDEX IF NOT EXISTS idx_batches_active_by_expiration
  ON batches (expires_at ASC)
  WHERE cancelling_at IS NULL;

COMMENT ON INDEX idx_batches_active_by_expiration IS
'Optimized for SLA-based queries: filters non-cancelled batches and sorts by expiration time';
