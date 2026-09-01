-- Remove deleted_at column from batches table
DROP INDEX IF EXISTS idx_batches_active_created_at_id;
ALTER TABLE batches DROP COLUMN IF EXISTS deleted_at;
