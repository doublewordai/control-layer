-- Add configurable auth header fields to inference_endpoints
ALTER TABLE inference_endpoints
ADD COLUMN auth_header_name VARCHAR NOT NULL DEFAULT 'Authorization',
ADD COLUMN auth_header_prefix VARCHAR NOT NULL DEFAULT 'Bearer ';

-- Update existing rows to use defaults (already set by DEFAULT clause above)
-- This ensures backward compatibility for existing endpoints
