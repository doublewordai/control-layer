-- Add model filtering support to inference endpoints
-- This allows endpoints to specify which models to sync

-- Add model_filter column to inference_endpoints table
-- This will store an array of model IDs to sync for this endpoint
-- If NULL, sync all models (default behavior)
ALTER TABLE inference_endpoints 
ADD COLUMN model_filter TEXT[] DEFAULT NULL;

-- Add index for efficient querying on model_filter
CREATE INDEX idx_inference_endpoints_model_filter 
ON inference_endpoints USING GIN (model_filter);

-- Add comment for clarity
COMMENT ON COLUMN inference_endpoints.model_filter IS 
'Array of model IDs to sync for this endpoint. NULL means sync all models.';