-- Candidate indexes for policy-driven content expiration. The migration is
-- intentionally limited to generic access paths; deployments choose their
-- own cutoffs through configuration.
--
-- On large production tables, create these indexes CONCURRENTLY before
-- deploying so the transactional migration's IF NOT EXISTS statements are
-- no-ops:
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
