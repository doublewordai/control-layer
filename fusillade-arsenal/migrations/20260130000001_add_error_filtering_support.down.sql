-- Rollback error filtering support

-- 1. Drop the index
DROP INDEX IF EXISTS idx_requests_failed_retriable;

-- 2. Drop the is_retriable_error column from requests
ALTER TABLE requests
DROP COLUMN IF EXISTS is_retriable_error;
