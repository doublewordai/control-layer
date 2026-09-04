-- no-transaction
-- File expiry by OpenAI-style expires_at.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_files_retention_due
    ON files (expires_at, id)
    WHERE deleted_at IS NULL AND expires_at IS NOT NULL
      AND status IN ('processed', 'expired');
