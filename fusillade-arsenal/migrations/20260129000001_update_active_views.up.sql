-- Update active_request_templates and active_requests views to filter on deleted_at
-- instead of relying on nullable foreign keys (ON DELETE SET NULL pattern)

-- Drop existing views
DROP VIEW IF EXISTS active_request_templates;
DROP VIEW IF EXISTS active_requests;

-- Recreate active_request_templates view
-- Filter based on parent file NOT being soft-deleted instead of FK IS NOT NULL
CREATE VIEW active_request_templates AS
SELECT rt.*
FROM request_templates rt
JOIN files f ON rt.file_id = f.id
WHERE f.deleted_at IS NULL;

-- Recreate active_requests view
-- Filter based on parent batch NOT being soft-deleted instead of FK IS NOT NULL
CREATE VIEW active_requests AS
SELECT r.*
FROM requests r
JOIN batches b ON r.batch_id = b.id
WHERE b.deleted_at IS NULL;
