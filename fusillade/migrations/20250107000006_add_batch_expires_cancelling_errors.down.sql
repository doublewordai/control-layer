-- Remove expires_at, cancelling_at, and errors fields from batches table

DROP INDEX IF EXISTS idx_batches_expires_at;

ALTER TABLE batches DROP COLUMN IF EXISTS errors;
ALTER TABLE batches DROP COLUMN IF EXISTS cancelling_at;
ALTER TABLE batches DROP COLUMN IF EXISTS expires_at;
