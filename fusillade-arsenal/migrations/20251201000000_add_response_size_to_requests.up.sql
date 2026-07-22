-- Add response_size column to requests table to track individual response sizes
-- This avoids expensive file size recomputation and eliminates trigger-based row updates

ALTER TABLE requests
ADD COLUMN response_size BIGINT NOT NULL DEFAULT 0;

-- Add comment explaining the column
COMMENT ON COLUMN requests.response_size IS 
    'Size in bytes of the response body for this request. Used by application code to estimate JSONL output/error file sizes.';

-- Add index to efficiently sum response sizes by batch and state
CREATE INDEX idx_requests_batch_state_size 
ON requests(batch_id, state)
INCLUDE (response_size);

-- Add flag to track if file size has been finalized
-- This prevents re-aggregation on every fetch for completed batches

ALTER TABLE files
ADD COLUMN size_finalized BOOLEAN NOT NULL DEFAULT FALSE;

-- Partial index for finding unfinalized virtual batch files
-- These are the only files where we need to calculate size on-the-fly
CREATE INDEX idx_files_unfinalized_batch_files
ON files(purpose, size_finalized)
WHERE size_finalized = FALSE 
  AND purpose IN ('batch_output', 'batch_error');

COMMENT ON COLUMN files.size_finalized IS 
    'Whether the size_bytes value has been finalized from request response_size aggregation. FALSE means size needs to be calculated/cached, TRUE means size_bytes is final.';