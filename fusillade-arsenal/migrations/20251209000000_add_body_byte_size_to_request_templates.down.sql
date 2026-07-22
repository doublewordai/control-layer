-- Rollback: Remove body_byte_size column and index

DROP INDEX IF EXISTS idx_request_templates_file_model;

ALTER TABLE request_templates
DROP COLUMN body_byte_size;
