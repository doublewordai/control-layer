-- Add column for storing custom_id from fusillade requests
-- This enables searching analytics by the user-provided custom_id

ALTER TABLE http_analytics ADD COLUMN custom_id TEXT;

-- Index for efficient searches by custom_id
CREATE INDEX idx_analytics_custom_id ON http_analytics (custom_id)
WHERE custom_id IS NOT NULL;
