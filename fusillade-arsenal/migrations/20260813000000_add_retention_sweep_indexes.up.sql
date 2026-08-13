-- Candidate indexes for policy-driven content expiration. The migration is
-- intentionally limited to generic access paths; deployments choose their
-- own cutoffs through configuration.
--
-- On large production tables, create these indexes CONCURRENTLY before
-- deploying so the transactional migration's IF NOT EXISTS statements are
-- no-ops. Run these with the same component-schema search_path used by the
-- application, or schema-qualify both index and table names explicitly:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_files_retention_due
--   ON files (expires_at, id)
--   WHERE deleted_at IS NULL AND expires_at IS NOT NULL
--     AND status IN ('processed', 'expired');
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_retention_due
--   ON batches (counts_frozen_at, id)
--   WHERE deleted_at IS NULL AND counts_frozen_at IS NOT NULL;
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_batchless_retention_due
--   ON requests (
--       service_tier,
--       (CASE state
--          WHEN 'completed' THEN completed_at
--          WHEN 'failed' THEN failed_at
--          WHEN 'canceled' THEN canceled_at
--        END),
--       id
--   )
--   WHERE batch_id IS NULL AND state IN ('completed', 'failed', 'canceled');
--
-- Verify every pre-created index is usable before deployment (replace
-- <component_schema> when it is not the current schema):
--
--   SELECT c.relname, i.indisready, i.indisvalid
--   FROM pg_index i
--   JOIN pg_class c ON c.oid = i.indexrelid
--   JOIN pg_namespace n ON n.oid = c.relnamespace
--   WHERE n.nspname = '<component_schema>'
--     AND c.relname IN (
--       'idx_files_retention_due',
--       'idx_batches_retention_due',
--       'idx_requests_batchless_retention_due'
--     );
--
-- All three rows must report indisready = true and indisvalid = true. An
-- invalid same-name index would make IF NOT EXISTS a no-op without providing
-- a usable access path and must be repaired before this migration runs.

CREATE INDEX IF NOT EXISTS idx_files_retention_due
ON files (expires_at, id)
WHERE deleted_at IS NULL
  AND expires_at IS NOT NULL
  AND status IN ('processed', 'expired');

CREATE INDEX IF NOT EXISTS idx_batches_retention_due
ON batches (counts_frozen_at, id)
WHERE deleted_at IS NULL
  AND counts_frozen_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_batchless_retention_due
ON requests (
    service_tier,
    (CASE state
       WHEN 'completed' THEN completed_at
       WHEN 'failed' THEN failed_at
       WHEN 'canceled' THEN canceled_at
     END),
    id
)
WHERE batch_id IS NULL
  AND state IN ('completed', 'failed', 'canceled');
