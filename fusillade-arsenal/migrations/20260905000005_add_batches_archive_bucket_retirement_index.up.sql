-- no-transaction
-- Retention stamp after an archive week retires.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_archive_bucket_retirement
    ON batches (archive_bucket, id)
    WHERE archive_bucket IS NOT NULL;
