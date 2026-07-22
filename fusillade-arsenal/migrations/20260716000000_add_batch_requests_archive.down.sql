DROP FUNCTION IF EXISTS ensure_archive_partitions(integer);
DROP TABLE IF EXISTS batch_requests_archive;  -- drops all partitions with it
ALTER TABLE batches
    DROP CONSTRAINT IF EXISTS batches_archived_have_bucket;
ALTER TABLE batches
    DROP COLUMN IF EXISTS location,
    DROP COLUMN IF EXISTS archive_bucket;
