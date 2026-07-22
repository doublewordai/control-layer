-- Remove OpenAI-compatible fields from batches table

DROP INDEX IF EXISTS idx_batches_error_file_id;
DROP INDEX IF EXISTS idx_batches_output_file_id;

ALTER TABLE batches DROP COLUMN IF EXISTS error_file_id;
ALTER TABLE batches DROP COLUMN IF EXISTS output_file_id;
ALTER TABLE batches DROP COLUMN IF EXISTS endpoint;
ALTER TABLE batches DROP COLUMN IF EXISTS completion_window;
ALTER TABLE batches DROP COLUMN IF EXISTS metadata;
