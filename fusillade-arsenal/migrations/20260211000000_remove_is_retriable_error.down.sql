-- Restore is_retriable_error column and index
-- Note: This only restores the schema, not the data.
-- Historical values will be NULL until requests fail again.

-- 1. Add is_retriable_error column to requests table
ALTER TABLE requests
ADD COLUMN is_retriable_error BOOLEAN NULL;

-- 2. Create index for fast filtering on failed requests by retryability
CREATE INDEX idx_requests_failed_retriable ON requests(batch_id, is_retriable_error) WHERE state = 'failed';