-- Add rate limiting support for API keys and deployed models

-- Add rate limiting columns to api_keys table
ALTER TABLE api_keys
ADD COLUMN requests_per_second REAL DEFAULT NULL,
ADD COLUMN burst_size INTEGER DEFAULT NULL;

-- Add rate limiting columns to deployed_models table
ALTER TABLE deployed_models
ADD COLUMN requests_per_second REAL DEFAULT NULL,
ADD COLUMN burst_size INTEGER DEFAULT NULL;

-- Add comments for clarity
COMMENT ON COLUMN api_keys.requests_per_second IS 'Per-API-key rate limit: tokens refilled per second (null = no limit)';
COMMENT ON COLUMN api_keys.burst_size IS 'Per-API-key rate limit: maximum tokens in bucket (null = no limit)';
COMMENT ON COLUMN deployed_models.requests_per_second IS 'Global per-model rate limit: tokens refilled per second (null = no limit)';
COMMENT ON COLUMN deployed_models.burst_size IS 'Global per-model rate limit: maximum tokens in bucket (null = no limit)';

-- Trigger auth config change notification for onwards config sync
NOTIFY auth_config_changed, 'rate_limits_added';