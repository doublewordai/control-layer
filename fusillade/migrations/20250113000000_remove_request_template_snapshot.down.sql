-- Restore ALL denormalized template fields to requests table

-- Drop the index on request_templates.model (we're reverting to denormalized)
DROP INDEX IF EXISTS idx_request_templates_model;

-- Add back the snapshot fields
ALTER TABLE requests
    ADD COLUMN endpoint TEXT,
    ADD COLUMN method TEXT,
    ADD COLUMN path TEXT,
    ADD COLUMN body TEXT DEFAULT '',
    ADD COLUMN model TEXT,
    ADD COLUMN api_key TEXT;

-- Repopulate from templates
UPDATE requests r
SET
    endpoint = t.endpoint,
    method = t.method,
    path = t.path,
    body = t.body,
    model = t.model,
    api_key = t.api_key
FROM request_templates t
WHERE r.template_id = t.id;

-- Make columns NOT NULL after population
ALTER TABLE requests
    ALTER COLUMN endpoint SET NOT NULL,
    ALTER COLUMN method SET NOT NULL,
    ALTER COLUMN path SET NOT NULL,
    ALTER COLUMN body SET NOT NULL,
    ALTER COLUMN model SET NOT NULL,
    ALTER COLUMN api_key SET NOT NULL;

-- Recreate the index on requests.model
CREATE INDEX idx_requests_model ON requests(model);
