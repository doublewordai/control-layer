DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM request_templates_retained LIMIT 1)
       OR EXISTS (
           SELECT 1 FROM request_templates_legacy
           WHERE retained_bucket IS NOT NULL
           LIMIT 1
       ) THEN
        RAISE EXCEPTION
            'cannot revert retention cutover after retained templates have been written';
    END IF;
END;
$$;

DROP VIEW active_request_templates;
DROP TRIGGER route_request_template_writes ON request_templates;
DROP FUNCTION route_request_template_write();
DROP VIEW request_templates;
DROP VIEW IF EXISTS request_templates_by_file;
DROP FUNCTION ensure_request_template_partitions(INTEGER);
DROP FUNCTION IF EXISTS ensure_request_template_partition(TIMESTAMPTZ, TEXT);
DROP FUNCTION IF EXISTS ensure_request_template_partition(TIMESTAMPTZ);
DROP FUNCTION request_template_legacy_retirement_ready(INTERVAL);
DROP TABLE request_template_storage_cutover;
DROP TABLE retention_partition_retirements;
DROP TABLE IF EXISTS request_template_retained_file_buckets;
DROP TABLE request_template_retained_keys;
DROP TABLE request_templates_retained;
ALTER TABLE request_templates_legacy DROP COLUMN retained_bucket;
ALTER TABLE request_templates_legacy RENAME TO request_templates;

CREATE VIEW active_request_templates AS
SELECT rt.*
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL;

DROP INDEX IF EXISTS idx_requests_batchless_retention_due;
DROP INDEX IF EXISTS idx_batches_retention_due;
DROP INDEX IF EXISTS idx_batches_archive_bucket_retirement;
DROP INDEX IF EXISTS idx_files_retention_due;
ALTER TABLE batches DROP COLUMN retention_expired_at;
ALTER TABLE files DROP COLUMN retention_expired_at;
