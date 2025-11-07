-- Add line_number to request_templates to maintain insertion order within a file
ALTER TABLE request_templates ADD COLUMN line_number INTEGER NOT NULL DEFAULT 0;

-- Create index for efficient ordering
CREATE INDEX idx_request_templates_file_line ON request_templates(file_id, line_number);
