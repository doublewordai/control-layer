-- Best-effort schema rollback. The data migration cannot be undone: virtual
-- batches deleted in the up.sql are gone, and there's no way to reconstruct
-- the (file, template, batch) trio from a batchless request.

DROP INDEX IF EXISTS idx_requests_user_active_sort;
DROP INDEX IF EXISTS idx_requests_user_created_sort;

ALTER TABLE requests DROP CONSTRAINT IF EXISTS requests_attribution_xor;
ALTER TABLE requests DROP COLUMN IF EXISTS created_by;

-- Restore the original views (inner-join variants).
DROP VIEW IF EXISTS active_request_templates;
CREATE VIEW active_request_templates AS
SELECT rt.*
FROM request_templates rt
JOIN files f ON rt.file_id = f.id
WHERE f.deleted_at IS NULL;

DROP VIEW IF EXISTS active_requests;
CREATE VIEW active_requests AS
SELECT r.*
FROM requests r
JOIN batches b ON r.batch_id = b.id
WHERE b.deleted_at IS NULL;

-- batch_id NOT NULL is not restored here because batchless rows may exist.