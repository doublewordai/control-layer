-- Add time-to-first-byte column for OpenTelemetry GenAI metrics
-- This enables the gen_ai.server.time_to_first_token metric

ALTER TABLE http_analytics
ADD COLUMN duration_to_first_byte_ms BIGINT;

-- Add index for TTFB-related queries
CREATE INDEX idx_analytics_ttfb ON http_analytics (duration_to_first_byte_ms)
WHERE duration_to_first_byte_ms IS NOT NULL;

-- Add comment for clarity
COMMENT ON COLUMN http_analytics.duration_to_first_byte_ms IS
'Time in milliseconds until first byte received from upstream provider (used for time-to-first-token metric)';