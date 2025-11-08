-- Add expires_at, cancelling_at, and errors fields to batches table

-- Add expires_at (calculated from created_at + completion_window)
ALTER TABLE batches ADD COLUMN expires_at TIMESTAMPTZ NULL;

-- Add cancelling_at to track when cancellation was initiated
ALTER TABLE batches ADD COLUMN cancelling_at TIMESTAMPTZ NULL;

-- Add errors field for batch-level error tracking
ALTER TABLE batches ADD COLUMN errors JSONB NULL;

-- Create index for filtering by expiration
CREATE INDEX idx_batches_expires_at ON batches(expires_at) WHERE expires_at IS NOT NULL;
