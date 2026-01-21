-- Add is_internal column to users table
-- Marks users as internal (e.g., system users, service accounts)
ALTER TABLE users ADD COLUMN is_internal BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN users.is_internal IS 'Whether this user is an internal user (e.g., system user, service account)';

-- Add batch_metadata_request_origin column to http_analytics table
-- Stores the request origin from batch metadata
ALTER TABLE http_analytics ADD COLUMN batch_metadata_request_origin TEXT NOT NULL DEFAULT '';

-- Index for efficient filtering by batch_metadata_request_origin (only non-empty values)
CREATE INDEX idx_analytics_batch_metadata_request_origin ON http_analytics (batch_metadata_request_origin)
WHERE batch_metadata_request_origin != '';
