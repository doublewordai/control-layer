ALTER TABLE batches
    DROP COLUMN IF EXISTS completed_requests,
    DROP COLUMN IF EXISTS failed_requests,
    DROP COLUMN IF EXISTS canceled_requests;
ALTER TABLE batches
    DROP COLUMN IF EXISTS counts_frozen_at;
ALTER TABLE batches
    DROP COLUMN IF EXISTS retry_version;
