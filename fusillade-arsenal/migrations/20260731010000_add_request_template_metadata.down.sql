-- Rebuild the view without the column first: a view holding `metadata` blocks the DROP.
DROP VIEW IF EXISTS active_request_templates;
CREATE VIEW active_request_templates AS
SELECT rt.id, rt.file_id, rt.endpoint, rt.method, rt.path, rt.body, rt.model, rt.api_key,
       rt.created_at, rt.updated_at, rt.custom_id, rt.line_number, rt.body_byte_size
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL;

ALTER TABLE request_templates DROP COLUMN IF EXISTS metadata;
