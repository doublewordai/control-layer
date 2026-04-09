-- Track ingestion quality per sync entry
ALTER TABLE sync_entries
    ADD COLUMN skipped_lines INT NOT NULL DEFAULT 0,
    ADD COLUMN validation_errors JSONB;
-- validation_errors schema: [{"line": 3, "error": "missing model field"}, ...]
