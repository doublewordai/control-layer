-- Remove ALL denormalized template fields from requests table
-- All template data will now be fetched via JOIN with request_templates

-- Remove the snapshot fields - template_id FK provides the relationship
ALTER TABLE requests
    DROP COLUMN endpoint,
    DROP COLUMN method,
    DROP COLUMN path,
    DROP COLUMN body,
    DROP COLUMN model,  -- This also drops idx_requests_model automatically
    DROP COLUMN api_key;

-- Add index on request_templates.model for claim_requests performance
-- (claim_requests uses PARTITION BY model via JOIN)
CREATE INDEX IF NOT EXISTS idx_request_templates_model ON request_templates(model);

-- Note: template_id FK (idx_requests_template_id) already exists for efficient JOINs
