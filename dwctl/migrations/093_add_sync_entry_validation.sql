-- Track ingestion quality per sync entry
ALTER TABLE sync_entries
    ADD COLUMN skipped_lines INT NOT NULL DEFAULT 0,
    ADD COLUMN validation_errors JSONB;
-- validation_errors schema: [{"template_index": 0, "line": 3, "error": "missing model field"}, ...]
-- template_index: 0-based position among ingested templates (matches request_templates.line_number)
-- line: 1-based source file line number (for UI display)
