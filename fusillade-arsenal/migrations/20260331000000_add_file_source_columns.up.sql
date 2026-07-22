-- Track provenance of externally-sourced files
ALTER TABLE files
    ADD COLUMN source_connection_id UUID,
    ADD COLUMN source_external_key  TEXT;

CREATE INDEX idx_files_source_connection
    ON files (source_connection_id)
    WHERE source_connection_id IS NOT NULL;
