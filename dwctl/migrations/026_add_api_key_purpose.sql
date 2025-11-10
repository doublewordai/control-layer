-- Add purpose field to API keys to distinguish between platform and inference access
-- Platform keys can access /admin/api/* endpoints (management APIs)
-- Inference keys can access /ai/* endpoints (AI/inference endpoints)

-- Add purpose column to api_keys table with CHECK constraint
-- Default existing keys to 'inference' since that's the current behavior
ALTER TABLE api_keys
ADD COLUMN purpose VARCHAR NOT NULL DEFAULT 'inference'
CHECK (purpose IN ('platform', 'inference'));

-- Remove the default after setting existing rows
-- Future inserts must explicitly specify the purpose
ALTER TABLE api_keys
ALTER COLUMN purpose DROP DEFAULT;
