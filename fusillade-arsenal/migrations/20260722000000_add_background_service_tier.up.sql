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
    ADD COLUMN service_tier TEXT;

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

-- The requests indexes live in the following no-transaction migrations so
-- PostgreSQL can build them concurrently without blocking writes.
CREATE INDEX IF NOT EXISTS idx_batches_background_active
ON batches (created_at, id)
WHERE service_tier = 'background'
  AND deleted_at IS NULL;
