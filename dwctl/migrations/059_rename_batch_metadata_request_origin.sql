-- Rename batch_metadata_request_origin to batch_request_source
-- This better reflects that it stores the request_source from batch metadata

-- Drop the old index (not needed)
DROP INDEX IF EXISTS idx_analytics_batch_metadata_request_origin;

-- Rename the column
ALTER TABLE http_analytics RENAME COLUMN batch_metadata_request_origin TO batch_request_source;
