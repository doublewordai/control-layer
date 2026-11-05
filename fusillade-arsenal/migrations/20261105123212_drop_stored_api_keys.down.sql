SET LOCAL lock_timeout = '5s';

-- Restore the columns (empty, as the previous release writes them) and the
-- views' previous definitions (20261102120000, 20261008130000). The secrets
-- themselves are gone.
ALTER TABLE batches ADD COLUMN api_key TEXT;
ALTER TABLE request_templates_g2 ADD COLUMN api_key TEXT NOT NULL DEFAULT '';

DROP VIEW active_request_templates;
DROP VIEW request_templates_all;
DROP VIEW request_templates;

CREATE VIEW active_request_templates AS
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata, g2.api_key_id
FROM request_template_routes route
JOIN request_template_buckets bucket
  ON bucket.week_start = route.week_start
 AND bucket.state = 'active'
JOIN request_templates_g2 g2
  ON g2.created_on >= route.week_start
 AND g2.created_on < route.week_start + 7
 AND g2.id = route.template_id
LEFT JOIN files f ON g2.file_id = f.id
WHERE g2.file_id IS NULL OR f.deleted_at IS NULL;

CREATE VIEW request_templates_all AS
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata, g2.api_key_id
FROM request_templates_g2 g2;

CREATE VIEW request_templates AS
SELECT id, file_id, endpoint, method, path, body, model, api_key, created_at,
       updated_at, custom_id, line_number, body_byte_size, metadata
FROM request_templates_g2
WHERE false
WITH CASCADED CHECK OPTION;
