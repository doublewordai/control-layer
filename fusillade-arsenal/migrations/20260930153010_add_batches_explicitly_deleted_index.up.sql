-- no-transaction
--
-- Driving set for the batch half of purge_orphaned_rows.
--
-- Both batch purges (request rows, then archived rows) start from
--
--   SELECT id[, archive_bucket] FROM batches
--   WHERE deleted_at IS NOT NULL AND retention_expired_at IS NULL
--
-- and then seek each batch's rows by batch_id. Nothing indexes that
-- predicate, so every purge pass scanned all of `batches` to find a handful of
-- explicitly deleted batches, on a daemon tick.
--
-- The predicate is exactly those static conditions, so generic plans can use
-- it. `archive_bucket` is included so the archive purge, which also filters
-- `archive_bucket IS NOT NULL` and needs the bucket for partition pruning, is
-- answered index-only as well. Only explicitly deleted batches qualify:
-- retention expiry stamps retention_expired_at and leaves deleted_at NULL, so
-- aged-out batches never enter.
--
-- One statement only: CONCURRENTLY cannot run in a transaction. The following
-- migrations reindex (repairing an interrupted build) and validate it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_explicitly_deleted
ON batches (id) INCLUDE (archive_bucket)
WHERE deleted_at IS NOT NULL
  AND retention_expired_at IS NULL;
