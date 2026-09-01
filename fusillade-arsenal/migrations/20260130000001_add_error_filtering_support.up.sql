-- Add support for filtering errors by retryability
-- This migration adds is_retriable_error to enable on-demand counting of retriable vs non-retriable failures
-- Note: No backfill for historical data - historical failures will have NULL is_retriable_error
-- and be treated as non-retriable for filtering; reliable retryability classification only
-- applies to new failures

-- 1. Add is_retriable_error column to requests table
-- NULL for historical requests, will be set for new failures
ALTER TABLE requests
ADD COLUMN is_retriable_error BOOLEAN NULL;

-- 2. Create index for fast filtering on failed requests by retryability
-- This enables efficient on-demand counting: COUNT(*) FILTER (WHERE state = 'failed' AND is_retriable_error = true/false/NULL)
CREATE INDEX idx_requests_failed_retriable ON requests(batch_id, is_retriable_error) WHERE state = 'failed';
