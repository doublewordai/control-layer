-- Drop the API key secrets stored on request templates (contract step 2 of 2).
--
-- Requests are dispatched by API key id, and the previous release neither
-- reads nor writes this column: new templates took the empty default. This
-- removes the column from the live schema; batches.api_key goes in
-- 20261105143830. DROP COLUMN rewrites nothing: the old bytes stay in table
-- pages until rows are rewritten or their weekly partition retires, and in
-- WAL and backups until they expire.
--
-- CREATE OR REPLACE cannot remove a view column, so the views that project
-- api_key are recreated without it first. Every statement is metadata-only;
-- DROP COLUMN recurses to every weekly partition.
SET LOCAL lock_timeout = '5s';

DROP VIEW active_request_templates;
DROP VIEW request_templates_all;
DROP VIEW request_templates;

CREATE VIEW active_request_templates AS
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
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
       g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata, g2.api_key_id
FROM request_templates_g2 g2;

-- The empty compatibility view kept for drained generation-2 writers
-- (20261008130000), without the key.
CREATE VIEW request_templates AS
SELECT id, file_id, endpoint, method, path, body, model, created_at,
       updated_at, custom_id, line_number, body_byte_size, metadata
FROM request_templates_g2
WHERE false
WITH CASCADED CHECK OPTION;

ALTER TABLE request_templates_g2 DROP COLUMN api_key;
