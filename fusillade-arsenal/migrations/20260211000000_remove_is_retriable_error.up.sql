-- Remove is_retriable_error column and index
-- This column was added for error filtering feature which has been removed.
-- The retry logic now retries ALL failed requests regardless of error type,
-- and error classification is available via FailureReason::is_retriable() if needed.

-- 1. Drop the index
DROP INDEX IF EXISTS idx_requests_failed_retriable;

-- 2. Drop the is_retriable_error column from requests
ALTER TABLE requests
DROP COLUMN IF EXISTS is_retriable_error;