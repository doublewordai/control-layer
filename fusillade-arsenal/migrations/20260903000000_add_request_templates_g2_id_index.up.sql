-- Generation-2 templates are keyed by (created_on, id) so the primary key
-- serves partition-pruned scans, but the routed read paths probe a single
-- template by id within its week. Without an id-leading index that probe is a
-- range scan of the whole week's primary key on PostgreSQL < 18 (no skip
-- scan). Partitioned index: existing and future weekly children inherit it.
CREATE INDEX IF NOT EXISTS idx_request_templates_g2_id
    ON request_templates_g2 (id);

-- Operator note (not built here, like the other optional retention indexes):
-- scheduled file-content expiry (expire_file_content) selects by upload age,
-- not by the OpenAI-style files.expires_at, so it needs its own index:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_files_content_expiry_due
--     ON files (created_at, id)
--     WHERE purpose = 'batch' AND deleted_at IS NULL;
