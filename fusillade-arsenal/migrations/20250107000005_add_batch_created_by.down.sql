-- Remove created_by field from batches table

DROP INDEX IF EXISTS idx_batches_created_by;
ALTER TABLE batches DROP COLUMN IF EXISTS created_by;
