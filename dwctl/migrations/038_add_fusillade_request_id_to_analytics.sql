-- Add column for linking http_analytics to fusillade requests
-- This enables aggregating batch request metrics by joining through the request ID

ALTER TABLE http_analytics ADD COLUMN fusillade_request_id UUID;

-- Index for efficient joins between http_analytics and fusillade.requests
CREATE INDEX idx_analytics_fusillade_request_id ON http_analytics (fusillade_request_id)
WHERE fusillade_request_id IS NOT NULL;
