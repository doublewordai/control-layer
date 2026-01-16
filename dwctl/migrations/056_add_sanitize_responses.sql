-- Add sanitize_responses column to deployed_models table
-- When enabled, onwards will sanitize/filter sensitive data from responses

ALTER TABLE deployed_models ADD COLUMN sanitize_responses BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN deployed_models.sanitize_responses IS 'Whether to sanitize/filter sensitive data from model responses (default: false)';