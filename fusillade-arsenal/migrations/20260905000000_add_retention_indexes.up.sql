-- The permanent indexes behind scheduled content retention: the steady sweep
-- and backfill candidate scan, file expiry, and the weekly retirement gates.
-- They were previously documented as operator-built statements; they belong
-- in the schema.
--
-- Built here with plain CREATE INDEX IF NOT EXISTS (transactional). On a
-- production-sized requests table the first index takes several minutes and
-- blocks writes for the build, so on staging and production pre-build the
-- identical definitions with CREATE INDEX CONCURRENTLY before deploying
-- this release; the migration then finds them and does nothing. Fresh and
-- small databases (tests, previews) build them inline.
CREATE INDEX IF NOT EXISTS idx_requests_batchless_retention_due
    ON requests (
        service_tier,
        (CASE state WHEN 'completed' THEN completed_at
                    WHEN 'failed' THEN failed_at
                    WHEN 'canceled' THEN canceled_at END),
        id
    )
    WHERE batch_id IS NULL
      AND state IN ('completed', 'failed', 'canceled');

CREATE INDEX IF NOT EXISTS idx_files_retention_due
    ON files (expires_at, id)
    WHERE deleted_at IS NULL AND expires_at IS NOT NULL
      AND status IN ('processed', 'expired');

CREATE INDEX IF NOT EXISTS idx_files_content_expiry_due
    ON files (created_at, id)
    WHERE purpose = 'batch' AND deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_batches_retention_due
    ON batches (counts_frozen_at, id)
    WHERE deleted_at IS NULL AND counts_frozen_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_batches_archive_bucket_retirement
    ON batches (archive_bucket, id)
    WHERE archive_bucket IS NOT NULL;
