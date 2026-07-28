-- Background work has no completion SLA. It can be submitted either as a
-- file-backed batch or as a batchless request, but both forms use the same
-- request-level service tier and scheduling path.

ALTER TABLE requests
    DROP CONSTRAINT IF EXISTS requests_service_tier_check;
ALTER TABLE requests
    ADD CONSTRAINT requests_service_tier_check
    CHECK (service_tier IN ('auto', 'default', 'flex', 'priority', 'background'))
    NOT VALID;

-- The archive mirrors requests constraints. Terminal background batch rows
-- must remain archivable after they complete.
ALTER TABLE batch_requests_archive
    DROP CONSTRAINT IF EXISTS requests_service_tier_check;
ALTER TABLE batch_requests_archive
    ADD CONSTRAINT requests_service_tier_check
    CHECK (service_tier IN ('auto', 'default', 'flex', 'priority', 'background'))
    NOT VALID;

ALTER TABLE batches
    ADD COLUMN IF NOT EXISTS service_tier TEXT;

ALTER TABLE batches
    ALTER COLUMN completion_window DROP NOT NULL,
    ALTER COLUMN expires_at DROP NOT NULL;

ALTER TABLE batches
    ADD CONSTRAINT batches_service_tier_check
    CHECK (service_tier IS NULL OR service_tier = 'background') NOT VALID;

ALTER TABLE batches
    ADD CONSTRAINT batches_background_deadline_check
    CHECK (
        CASE
            WHEN service_tier = 'background' THEN
                completion_window IS NULL AND expires_at IS NULL
            WHEN service_tier IS NULL THEN
                completion_window IS NOT NULL AND expires_at IS NOT NULL
            ELSE FALSE
        END
    ) NOT VALID;

CREATE INDEX IF NOT EXISTS idx_batches_background_active
ON batches (created_at, id)
WHERE service_tier = 'background'
  AND deleted_at IS NULL;

-- Background batchless requests have no batch parent and are claimed by model.
CREATE INDEX IF NOT EXISTS idx_requests_pending_background_batchless
ON requests (model, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NULL
  AND template_id IS NOT NULL
  AND service_tier = 'background';

-- File-backed background requests retain their batch parent, so keep this hot
-- claim path separate from the batchless source.
CREATE INDEX IF NOT EXISTS idx_requests_pending_background_batched
ON requests (model, batch_id, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NOT NULL
  AND template_id IS NOT NULL
  AND service_tier = 'background';

-- Keep the existing SLA batchless claim from scanning a large background
-- backlog once background submission is enabled.
CREATE INDEX IF NOT EXISTS idx_requests_pending_batchless_sla
ON requests (model, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NULL
  AND template_id IS NOT NULL
  AND service_tier IS DISTINCT FROM 'background';

-- Background capacity is derived from active SLA work, excluding priority and
-- background rows from that count.
CREATE INDEX IF NOT EXISTS idx_requests_active_sla_counts
ON requests (batch_id, model)
WHERE state IN ('pending', 'claimed', 'processing')
  AND template_id IS NOT NULL
  AND (
      service_tier IS NULL
      OR service_tier NOT IN ('priority', 'background')
  );
