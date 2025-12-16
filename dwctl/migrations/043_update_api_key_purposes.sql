-- Update API key purposes to distinguish between different inference key types
-- This migration:
-- 1. Updates CHECK constraint to allow new purpose values
-- 2. Renames user-created 'inference' keys to 'realtime'
-- 3. Renames hidden 'inference' keys to 'batch' (backwards compatible)
-- 4. Prepares for new 'playground' purpose (will be created on-demand)

-- Step 1: Drop old CHECK constraint and add new one with updated values
ALTER TABLE api_keys DROP CONSTRAINT api_keys_purpose_check;
ALTER TABLE api_keys
ADD CONSTRAINT api_keys_purpose_check
CHECK (purpose IN ('platform', 'inference', 'realtime', 'batch', 'playground'));

-- Step 2: Update existing user-created 'inference' keys to 'realtime'
UPDATE api_keys
SET purpose = 'realtime'
WHERE purpose = 'inference' AND hidden = false;

-- Step 2: Update existing hidden 'inference' keys to 'batch'
-- These are the keys already embedded in request_templates in fusillade
-- We preserve them to maintain compatibility with existing batch requests
UPDATE api_keys
SET purpose = 'batch'
WHERE purpose = 'inference' AND hidden = true;

-- Step 3: Update the unique index to allow multiple hidden key purposes per user
-- Each user can now have one hidden key per purpose: 'batch' and 'playground'
DROP INDEX IF EXISTS idx_api_keys_user_hidden_purpose;
CREATE UNIQUE INDEX idx_api_keys_user_hidden_purpose
  ON api_keys(user_id, purpose)
  WHERE hidden = true;

-- Note: The purpose column is already VARCHAR, so no schema change needed
-- The enum values are enforced at the application level via sqlx::Type
-- New 'playground' hidden keys will be created on-demand when users use the playground