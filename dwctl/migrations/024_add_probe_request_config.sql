-- Add configurable request path, body, and HTTP method to probes
ALTER TABLE probes
ADD COLUMN http_method TEXT NOT NULL DEFAULT 'POST',
ADD COLUMN request_path TEXT,
ADD COLUMN request_body JSONB;

-- Add comment explaining the fields
COMMENT ON COLUMN probes.http_method IS 'HTTP method to use for the probe request (GET, POST, etc.)';
COMMENT ON COLUMN probes.request_path IS 'Path to append to the endpoint URL (e.g., /v1/chat/completions)';
COMMENT ON COLUMN probes.request_body IS 'JSON body to send with the probe request';

-- Update existing probes to have default values based on typical OpenAI-compatible endpoints
-- For now, we'll leave them NULL and the executor will fall back to the old hardcoded behavior
-- This allows for a smooth migration
