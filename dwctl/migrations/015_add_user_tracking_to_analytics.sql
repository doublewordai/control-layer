-- Add user tracking and pricing information to http_analytics table

-- Add user_id column to track which user made the request
ALTER TABLE http_analytics
ADD COLUMN user_id UUID REFERENCES users(id) ON DELETE SET NULL;

-- Add access_source to track whether request came from Playground (SSO) or API key
ALTER TABLE http_analytics
ADD COLUMN access_source VARCHAR(20);

-- Add model pricing columns (cached at request time for historical accuracy)
ALTER TABLE http_analytics
ADD COLUMN input_price_per_token DECIMAL(12, 8),
ADD COLUMN output_price_per_token DECIMAL(12, 8),
ADD COLUMN total_cost DECIMAL(12, 8) GENERATED ALWAYS AS (
    CASE
        WHEN prompt_tokens IS NOT NULL AND input_price_per_token IS NOT NULL
             AND completion_tokens IS NOT NULL AND output_price_per_token IS NOT NULL
        THEN (prompt_tokens * input_price_per_token) + (completion_tokens * output_price_per_token)
        ELSE NULL
    END
) STORED;

-- Add comments for clarity
COMMENT ON COLUMN http_analytics.user_id IS 'User who made the request (from API key or SSO header)';
COMMENT ON COLUMN http_analytics.access_source IS 'Source of authentication: "playground" (SSO), "api_key", or "system"';
COMMENT ON COLUMN http_analytics.input_price_per_token IS 'Price per input token at time of request';
COMMENT ON COLUMN http_analytics.output_price_per_token IS 'Price per output token at time of request';
COMMENT ON COLUMN http_analytics.total_cost IS 'Total cost of the request (auto-calculated)';

-- Add index for user-based queries
CREATE INDEX idx_analytics_user_id ON http_analytics (user_id, timestamp DESC) WHERE user_id IS NOT NULL;

-- Add index for access source analysis
CREATE INDEX idx_analytics_access_source ON http_analytics (access_source, timestamp DESC) WHERE access_source IS NOT NULL;

-- Add constraint for valid access sources
ALTER TABLE http_analytics
ADD CONSTRAINT check_access_source
CHECK (access_source IN ('playground', 'api_key', 'system') OR access_source IS NULL);