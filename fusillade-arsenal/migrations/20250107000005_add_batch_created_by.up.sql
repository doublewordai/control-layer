-- Add created_by field to batches table for ownership tracking

ALTER TABLE batches ADD COLUMN created_by TEXT NULL;

-- Create index for filtering by created_by
CREATE INDEX idx_batches_created_by ON batches(created_by) WHERE created_by IS NOT NULL;
