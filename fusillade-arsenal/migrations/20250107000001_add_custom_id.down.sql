-- Remove custom_id column from request_templates
DROP INDEX IF EXISTS idx_request_templates_custom_id;
ALTER TABLE request_templates DROP COLUMN IF EXISTS custom_id;

-- Remove custom_id from requests table
ALTER TABLE requests DROP COLUMN IF EXISTS custom_id;
