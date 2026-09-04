-- no-transaction
-- Weekly batch-archive retirement gate.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_retention_due
    ON batches (counts_frozen_at, id)
    WHERE deleted_at IS NULL AND counts_frozen_at IS NOT NULL;
