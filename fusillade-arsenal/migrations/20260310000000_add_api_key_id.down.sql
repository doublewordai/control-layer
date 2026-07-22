DROP INDEX IF EXISTS idx_files_api_key_id;
ALTER TABLE files DROP COLUMN IF EXISTS api_key_id;

DROP INDEX IF EXISTS idx_batches_api_key_id;
ALTER TABLE batches DROP COLUMN IF EXISTS api_key_id;
