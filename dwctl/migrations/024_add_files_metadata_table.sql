-- Add files metadata table for OpenAI-compatible files API
-- Files are uploaded, processed into requests immediately, then metadata retained

-- Create or replace the updated_at trigger function (if not already exists)
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Files table - stores metadata only
-- Content is processed immediately into requests table
CREATE TABLE files (
    id UUID PRIMARY KEY, -- ID provided by application (not auto-generated)
    filename TEXT NOT NULL,
    size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
    
    -- File status: 'processed', 'error', 'deleted', 'expired'
    status TEXT NOT NULL DEFAULT 'processed' CHECK (status IN ('processed', 'error', 'deleted', 'expired')),
    error_message TEXT, -- If status = 'error', store details here
    
    -- File lifecycle
    expires_at TIMESTAMPTZ, -- When file should expire (NULL = never expires)
    deleted_at TIMESTAMPTZ, -- When file was soft-deleted by user
    
    -- Ownership
    uploaded_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    
    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for efficient queries
CREATE INDEX idx_files_uploaded_by ON files(uploaded_by);
CREATE INDEX idx_files_created_at ON files(created_at DESC);
CREATE INDEX idx_files_status ON files(status);
CREATE INDEX idx_files_expires_at ON files(expires_at) WHERE expires_at IS NOT NULL AND status = 'processed';

-- Updated_at trigger
CREATE TRIGGER update_files_updated_at
    BEFORE UPDATE ON files
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Comments
COMMENT ON TABLE files IS 'File metadata for OpenAI-compatible files API. Content is processed immediately into requests table. ID is provided by application to link with requests.';
COMMENT ON COLUMN files.id IS 'Application-provided UUID to link file metadata with requests';
COMMENT ON COLUMN files.status IS 'File status: processed (successfully parsed into requests), error (failed to process), deleted (soft-deleted by user), expired (passed expiration date)';
COMMENT ON COLUMN files.error_message IS 'Error details if status = error';
COMMENT ON COLUMN files.expires_at IS 'When file should expire. NULL means never expires. Metadata retained for audit after expiration.';
COMMENT ON COLUMN files.deleted_at IS 'When user soft-deleted the file. Metadata retained for audit.';