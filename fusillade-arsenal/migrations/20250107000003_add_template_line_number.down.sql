-- Remove line_number from request_templates
DROP INDEX IF EXISTS idx_request_templates_file_line;
ALTER TABLE request_templates DROP COLUMN line_number;
