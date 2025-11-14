-- Add terminal state timestamps to batches table
-- These track when a batch enters its final state

ALTER TABLE batches
    ADD COLUMN finalizing_at TIMESTAMPTZ,
    ADD COLUMN completed_at TIMESTAMPTZ,
    ADD COLUMN failed_at TIMESTAMPTZ,
    ADD COLUMN cancelled_at TIMESTAMPTZ;

-- Add comment explaining the timestamps
COMMENT ON COLUMN batches.finalizing_at IS 'Timestamp when batch entered finalizing state (all requests terminal, output files being created)';
COMMENT ON COLUMN batches.completed_at IS 'Timestamp when batch completed successfully';
COMMENT ON COLUMN batches.failed_at IS 'Timestamp when batch failed';
COMMENT ON COLUMN batches.cancelled_at IS 'Timestamp when batch cancellation completed';
