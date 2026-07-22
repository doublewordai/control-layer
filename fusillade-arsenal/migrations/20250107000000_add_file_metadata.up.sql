-- Add metadata columns to files table for rich file tracking

ALTER TABLE files
    ADD COLUMN size_bytes BIGINT NOT NULL DEFAULT 0 CHECK (size_bytes >= 0),
    ADD COLUMN status TEXT NOT NULL DEFAULT 'processed' CHECK (status IN ('processed', 'error', 'deleted', 'expired')),
    ADD COLUMN error_message TEXT,
    ADD COLUMN purpose TEXT,
    ADD COLUMN expires_at TIMESTAMPTZ,
    ADD COLUMN deleted_at TIMESTAMPTZ,
    ADD COLUMN uploaded_by TEXT;

-- Indexes for efficient queries
CREATE INDEX idx_files_status ON files(status);
CREATE INDEX idx_files_expires_at ON files(expires_at) WHERE expires_at IS NOT NULL AND status = 'processed';
CREATE INDEX idx_files_uploaded_by ON files(uploaded_by) WHERE uploaded_by IS NOT NULL;

-- Comments
COMMENT ON COLUMN files.size_bytes IS 'Total size of the file in bytes';
COMMENT ON COLUMN files.status IS 'File status: processed (successfully parsed into templates), error (failed to process), deleted (soft-deleted by user), expired (passed expiration date)';
COMMENT ON COLUMN files.error_message IS 'Error details if status = error';
COMMENT ON COLUMN files.purpose IS 'File purpose (e.g., "batch" for batch API files)';
COMMENT ON COLUMN files.expires_at IS 'When file should expire. NULL means never expires. Metadata retained for audit after expiration.';
COMMENT ON COLUMN files.deleted_at IS 'When user soft-deleted the file. Metadata retained for audit.';
COMMENT ON COLUMN files.uploaded_by IS 'User identifier who uploaded the file (application-specific format)';
