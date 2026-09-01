-- SLA-based queue prioritization and monitoring
-- This migration adds:
-- 1. Indexes for SLA-based claim queries and monitoring
-- 2. NOT NULL constraint on expires_at (required for queue prioritization)

-- First, ensure there are no NULL expires_at values
-- If any exist, this will fail and prevent the migration - intentional!
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM batches WHERE expires_at IS NULL) THEN
        RAISE EXCEPTION 'Cannot make expires_at NOT NULL: found batches with NULL expires_at. These batches are unprocessable and should be investigated.';
    END IF;
END $$;

-- Add NOT NULL constraint to expires_at
-- Batches without expires_at will never be processed (NULLS LAST in queue)
ALTER TABLE batches ALTER COLUMN expires_at SET NOT NULL;

-- Drop the old partial index (only indexed non-NULL values)
DROP INDEX IF EXISTS idx_batches_expires_at;

-- Index on batches for SLA-based ordering (both claim_requests and find_at_risk_requests)
-- Filters non-cancelled batches and sorts by expiration time
CREATE INDEX idx_batches_active_by_expiration
ON batches(expires_at ASC)
WHERE cancelling_at IS NULL;

COMMENT ON INDEX idx_batches_active_by_expiration IS
'Optimized for SLA-based queries: filters non-cancelled batches and sorts by expiration time';

-- Index on requests for efficient filtering by pending state
-- Supports JOIN from requests to batches on batch_id
CREATE INDEX idx_requests_pending_by_batch
ON requests(batch_id, model)
WHERE state = 'pending';

COMMENT ON INDEX idx_requests_pending_by_batch IS
'Optimized for finding pending requests: supports batch JOIN and model-based filtering';
