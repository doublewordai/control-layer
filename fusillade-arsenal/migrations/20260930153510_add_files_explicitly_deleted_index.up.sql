-- no-transaction
--
-- Driving set for the template half of purge_orphaned_rows.
--
-- Both template purges (legacy request_templates and request_templates_g2)
-- start from
--
--   SELECT id FROM files
--   WHERE deleted_at IS NOT NULL AND retention_expired_at IS NULL
--
-- and then seek each file's templates by file_id. Nothing indexes that
-- predicate, so every purge pass scanned all of `files` to find the few
-- explicitly deleted files.
--
-- The predicate is exactly those static conditions, so generic plans can use
-- it, and the key answers the `id` projection index-only. File-content expiry
-- sets retention_expired_at together with deleted_at, so expired files never
-- enter; only explicit deletions do.
--
-- One statement only: CONCURRENTLY cannot run in a transaction. The following
-- migrations reindex (repairing an interrupted build) and validate it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_files_explicitly_deleted
ON files (id)
WHERE deleted_at IS NOT NULL
  AND retention_expired_at IS NULL;
