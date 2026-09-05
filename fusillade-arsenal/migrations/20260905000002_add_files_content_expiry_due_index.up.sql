-- no-transaction
-- Scheduled file-content expiry by upload age.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_files_content_expiry_due
    ON files (created_at, id)
    WHERE purpose = 'batch' AND deleted_at IS NULL;
