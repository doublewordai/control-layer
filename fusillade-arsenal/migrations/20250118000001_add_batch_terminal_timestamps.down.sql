-- Remove terminal state timestamps from batches table

ALTER TABLE batches
    DROP COLUMN IF EXISTS finalizing_at,
    DROP COLUMN IF EXISTS completed_at,
    DROP COLUMN IF EXISTS failed_at,
    DROP COLUMN IF EXISTS cancelled_at;
