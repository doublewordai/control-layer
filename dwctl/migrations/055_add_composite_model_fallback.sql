-- Add fallback configuration to composite models
-- Fallback determines behavior when a provider is unavailable

-- Load balancing strategy: weighted_random or priority
ALTER TABLE composite_models ADD COLUMN lb_strategy VARCHAR DEFAULT 'weighted_random';

-- Fallback configuration
ALTER TABLE composite_models ADD COLUMN fallback_enabled BOOLEAN DEFAULT TRUE;
ALTER TABLE composite_models ADD COLUMN fallback_on_rate_limit BOOLEAN DEFAULT TRUE;
-- JSON array of HTTP status codes that trigger fallback (e.g., [429, 500, 502, 503, 504])
ALTER TABLE composite_models ADD COLUMN fallback_on_status INTEGER[] DEFAULT '{429, 500, 502, 503, 504}';

COMMENT ON COLUMN composite_models.lb_strategy IS 'Load balancing strategy: weighted_random (default) or priority';
COMMENT ON COLUMN composite_models.fallback_enabled IS 'Whether to fall back to other providers when one fails';
COMMENT ON COLUMN composite_models.fallback_on_rate_limit IS 'Fall back when provider is rate limited';
COMMENT ON COLUMN composite_models.fallback_on_status IS 'HTTP status codes that trigger fallback to next provider';
