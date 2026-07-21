-- Revert SLA-based queue prioritization and monitoring changes
-- This reverses:
-- 1. Indexes for SLA-based queries
-- 2. NOT NULL constraint on expires_at

-- Remove indexes
DROP INDEX IF EXISTS idx_batches_active_by_expiration;
DROP INDEX IF EXISTS idx_requests_pending_by_batch;

-- Recreate the old partial index
CREATE INDEX idx_batches_expires_at ON batches(expires_at) WHERE expires_at IS NOT NULL;

-- Remove NOT NULL constraint from expires_at
ALTER TABLE batches ALTER COLUMN expires_at DROP NOT NULL;
