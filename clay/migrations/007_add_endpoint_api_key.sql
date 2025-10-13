-- Add API key support to inference endpoints

ALTER TABLE inference_endpoints 
ADD COLUMN api_key TEXT;

COMMENT ON COLUMN inference_endpoints.api_key IS 'Optional API key for authenticating with the inference endpoint (stored securely)';