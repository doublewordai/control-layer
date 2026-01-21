-- Rename batch_metadata_request_origin to batch_request_source
-- This better reflects that it stores the request_source from batch metadata

-- Drop the old index (not needed)
DROP INDEX IF EXISTS idx_analytics_batch_metadata_request_origin;

-- Rename the column
ALTER TABLE http_analytics RENAME COLUMN batch_metadata_request_origin TO batch_request_source;

-- Add column comments for documentation
-- request_origin: describes the source of the request based on API key purpose
COMMENT ON COLUMN http_analytics.request_origin IS 'Source of the request: "api" (standard API key), "frontend" (playground), or "fusillade" (batch processing)';

-- batch_sla and batch_request_source are fields from fusillade batch metadata
COMMENT ON COLUMN http_analytics.batch_sla IS 'Batch completion window from fusillade (e.g., "1h", "24h"). Empty string for non-batch requests.';
COMMENT ON COLUMN http_analytics.batch_request_source IS 'Request source from fusillade batch metadata (e.g., "api", "frontend"). Empty string for non-batch requests.';
