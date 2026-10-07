-- Keep both template generations readable while dedicated writes move to generation 2.
SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE VIEW active_request_templates AS
SELECT rt.id, rt.file_id, rt.endpoint, rt.method, rt.path, rt.body, rt.model,
       rt.api_key, rt.created_at, rt.updated_at, rt.custom_id, rt.line_number,
       rt.body_byte_size, rt.metadata
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL
UNION ALL
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
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

CREATE OR REPLACE VIEW request_templates_all AS
SELECT rt.id, rt.file_id, rt.endpoint, rt.method, rt.path, rt.body, rt.model,
       rt.api_key, rt.created_at, rt.updated_at, rt.custom_id, rt.line_number,
       rt.body_byte_size, rt.metadata
FROM request_templates rt
UNION ALL
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
FROM request_templates_g2 g2;
