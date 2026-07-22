-- Add body_byte_size column to request_templates for efficient cost estimation
-- This avoids needing to calculate LENGTH(body) on every aggregation query

-- Add the column with default 0 for existing rows (won't be accurate for existing data,
-- but avoids expensive backfill. New rows will have accurate values.)
ALTER TABLE request_templates
ADD COLUMN body_byte_size BIGINT NOT NULL DEFAULT 0;

-- Create index for efficient aggregation queries grouped by file_id and model
CREATE INDEX idx_request_templates_file_model ON request_templates(file_id, model);
