-- Improvements to user tracking in http_analytics table based on PR feedback

-- Add user_email column to store the email for easier identification
ALTER TABLE http_analytics
ADD COLUMN user_email VARCHAR(255);

-- Update constraint to include 'unknown_api_key' as a valid access_source
ALTER TABLE http_analytics
DROP CONSTRAINT IF EXISTS check_access_source;

ALTER TABLE http_analytics
ADD CONSTRAINT check_access_source
CHECK (access_source IN ('playground', 'api_key', 'unknown_api_key', 'unauthenticated') OR access_source IS NULL);

-- Add comment for the new column
COMMENT ON COLUMN http_analytics.user_email IS 'User email for easier identification (cached at request time)';

-- Add index for user email queries
CREATE INDEX idx_analytics_user_email ON http_analytics (user_email, timestamp DESC) WHERE user_email IS NOT NULL;