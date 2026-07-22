-- Re-add denormalized model column to requests table for efficient per-model batch limiting
-- This allows us to query and count requests by model without expensive JOINs

ALTER TABLE requests
    ADD COLUMN model TEXT;

-- Populate from templates
UPDATE requests r
SET model = t.model
FROM request_templates t
WHERE r.template_id = t.id;

-- Make column NOT NULL after population
ALTER TABLE requests
    ALTER COLUMN model SET NOT NULL;

-- Add index for efficient per-model queries
CREATE INDEX idx_requests_model ON requests(model);
