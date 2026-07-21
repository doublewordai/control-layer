-- Remove WAL system and switch to on-demand counting
-- This simplifies the system and eliminates counter drift issues

-- 1. Drop the WAL trigger
DROP TRIGGER IF EXISTS log_batch_state_change_to_wal ON requests;

-- 2. Drop the WAL trigger function
DROP FUNCTION IF EXISTS log_batch_state_change();

-- 3. Drop the compaction function
DROP FUNCTION IF EXISTS compact_batch_state_events(UUID);

-- 4. Drop the batch_state_events table
DROP TABLE IF EXISTS batch_state_events;

-- 5. Create composite index for efficient on-demand counting
-- This index allows index-only scans when counting request states
-- Note: Cannot use CONCURRENTLY in migrations (which run in transactions)
CREATE INDEX IF NOT EXISTS idx_requests_batch_state
ON requests(batch_id, state);

-- 6. Drop the now-unused counter columns from batches table
-- We'll compute these on-demand from the requests table
ALTER TABLE batches
    DROP COLUMN IF EXISTS pending_requests,
    DROP COLUMN IF EXISTS in_progress_requests,
    DROP COLUMN IF EXISTS completed_requests,
    DROP COLUMN IF EXISTS failed_requests,
    DROP COLUMN IF EXISTS canceled_requests,
    DROP COLUMN IF EXISTS requests_last_updated_at;

-- Note: total_requests and requests_started_at are still useful metadata, so we keep them
