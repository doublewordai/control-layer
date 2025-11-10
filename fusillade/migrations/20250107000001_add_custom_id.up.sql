-- Add custom_id column to request_templates for OpenAI Batch API compatibility
ALTER TABLE request_templates
ADD COLUMN custom_id TEXT;

-- Index for looking up templates by custom_id
CREATE INDEX idx_request_templates_custom_id ON request_templates(custom_id);

-- Add custom_id to requests table as well (denormalized snapshot)
ALTER TABLE requests
ADD COLUMN custom_id TEXT;
