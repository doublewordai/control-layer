-- Add deleted_at column to batches table for soft delete support
-- This enables proper soft deletes instead of orphaning child records via ON DELETE SET NULL

ALTER TABLE batches ADD COLUMN deleted_at TIMESTAMPTZ;

-- Partial index matching common access pattern: non-deleted batches ordered by creation time
-- This supports list_batches which filters by deleted_at IS NULL and orders by created_at DESC, id DESC
CREATE INDEX idx_batches_active_created_at_id
    ON batches (created_at DESC, id DESC)
    WHERE deleted_at IS NULL;

COMMENT ON COLUMN batches.deleted_at IS 'When batch was soft-deleted. NULL means active.';
