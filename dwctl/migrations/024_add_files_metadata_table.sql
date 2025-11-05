-- Add file storage metadata to main application database
-- Actual file contents stored elsewhere based on storage_backend

-- Create or replace the updated_at trigger function (if not already exists)
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- File status enum type
CREATE TYPE file_status AS ENUM ('active', 'deleted', 'expired', 'failed');

-- Main application metadata table
CREATE TABLE files (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    filename TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size_bytes BIGINT NOT NULL,
    
    -- Storage configuration
    storage_backend TEXT NOT NULL CHECK (storage_backend IN ('postgres', 'local')),
    storage_key TEXT NOT NULL, -- Backend-specific identifier: OID string (postgres) or relative path (local)

    -- File lifecycle
    status file_status NOT NULL DEFAULT 'active',
    expires_at TIMESTAMPTZ, -- When file should be deleted (NULL = never expires)
    deleted_at TIMESTAMPTZ, -- When file was soft-deleted by user
    
    -- File purpose and metadata
    purpose TEXT NOT NULL CHECK (purpose IN ('batch', 'batch_output')),
    uploaded_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    
    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for efficient queries
CREATE INDEX idx_files_uploaded_by ON files(uploaded_by);
CREATE INDEX idx_files_created_at ON files(created_at DESC);
CREATE INDEX idx_files_storage_backend ON files(storage_backend);
CREATE INDEX idx_files_purpose ON files(purpose);
CREATE INDEX idx_files_status ON files(status);
CREATE INDEX idx_files_expires_at ON files(expires_at) WHERE expires_at IS NOT NULL AND status = 'active';

-- Updated_at trigger
CREATE TRIGGER update_files_updated_at
    BEFORE UPDATE ON files
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Comments
COMMENT ON TABLE files IS 'File metadata. Actual file contents stored based on storage_backend configuration.';
COMMENT ON COLUMN files.storage_backend IS 'Storage backend type: postgres (large objects), s3 (object storage), or local (filesystem)';
COMMENT ON COLUMN files.storage_key IS 'Backend-specific identifier - Postgres: OID as string (e.g. "16384"), S3: object key (e.g. "files/abc-123.dat"), Local: relative path (e.g. "ab/abc-123.dat")';
COMMENT ON COLUMN files.purpose IS 'File purpose: batch (input for batch API) or batch_output (results from batch processing)';
COMMENT ON COLUMN files.status IS 'File lifecycle status: active (available), deleted (soft-deleted by user), expired (passed expiration date), failed (upload failed)';
COMMENT ON COLUMN files.expires_at IS 'When file content should be deleted. NULL means never expires. Content is deleted but metadata is retained for audit.';
COMMENT ON COLUMN files.deleted_at IS 'When user soft-deleted the file. Content is deleted but metadata is retained for audit.';