-- Restoring non-null batch deadlines would destroy background semantics.
-- Require operators to remove or deliberately migrate all background data
-- before rolling this schema back.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM batches WHERE service_tier = 'background')
       OR EXISTS (SELECT 1 FROM requests WHERE service_tier = 'background')
       OR EXISTS (
            SELECT 1
            FROM batch_requests_archive
            WHERE service_tier = 'background'
       ) THEN
        RAISE EXCEPTION
            'Cannot remove background service tier while background batches or requests remain';
    END IF;
END $$;

DROP INDEX IF EXISTS idx_batches_background_active;

ALTER TABLE batches
    DROP CONSTRAINT IF EXISTS batches_background_deadline_check,
    DROP CONSTRAINT IF EXISTS batches_service_tier_check;

ALTER TABLE batches
    ALTER COLUMN completion_window SET NOT NULL,
    ALTER COLUMN expires_at SET NOT NULL;

ALTER TABLE batches
    DROP COLUMN service_tier;

ALTER TABLE requests
    DROP CONSTRAINT IF EXISTS requests_service_tier_check;
ALTER TABLE requests
    ADD CONSTRAINT requests_service_tier_check
    CHECK (service_tier IN ('auto', 'default', 'flex', 'priority'))
    NOT VALID;

ALTER TABLE batch_requests_archive
    DROP CONSTRAINT IF EXISTS requests_service_tier_check;
ALTER TABLE batch_requests_archive
    ADD CONSTRAINT requests_service_tier_check
    CHECK (service_tier IN ('auto', 'default', 'flex', 'priority'))
    NOT VALID;
