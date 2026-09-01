-- Revert to original views that filter on FK IS NOT NULL

DROP VIEW IF EXISTS active_request_templates;
DROP VIEW IF EXISTS active_requests;

-- Restore original active_request_templates view
CREATE VIEW active_request_templates AS
SELECT * FROM request_templates WHERE file_id IS NOT NULL;

-- Restore original active_requests view
CREATE VIEW active_requests AS
SELECT * FROM requests WHERE batch_id IS NOT NULL;
