DROP INDEX IF EXISTS idx_files_source_connection;

ALTER TABLE files
    DROP COLUMN IF EXISTS source_external_key,
    DROP COLUMN IF EXISTS source_connection_id;
