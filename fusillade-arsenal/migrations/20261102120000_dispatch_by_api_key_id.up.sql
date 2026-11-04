-- Dispatch by API key id instead of a stored key secret (expand step).
--
-- A batchless template (flex, background) records the id of the key its
-- request runs as; a batch request already has one on `batches.api_key_id`.
-- The consuming application authenticates each dispatch from that id. The
-- key secret is still written alongside it, so the previous release can
-- serve every row this one writes; a later release stops writing it.
--
-- The id lives on the template, not on `requests`: `batch_requests_archive`
-- mirrors `requests` column for column with its bucket appended last, so a
-- column added to `requests` could not be mirrored there.
--
-- Every statement is metadata-only. ADD COLUMN recurses to every weekly
-- partition, and partitions created later copy the parent's columns. The
-- views gain the column at the end, which CREATE OR REPLACE allows and which
-- leaves the previous release's queries valid.
SET LOCAL lock_timeout = '5s';

ALTER TABLE request_templates_g2 ADD COLUMN api_key_id UUID;

COMMENT ON COLUMN request_templates_g2.api_key_id IS
    'API key a batchless request is dispatched as. NULL for file templates: a batch request uses batches.api_key_id.';

CREATE OR REPLACE VIEW active_request_templates AS
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

CREATE OR REPLACE VIEW request_templates_all AS
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata, g2.api_key_id
FROM request_templates_g2 g2;
