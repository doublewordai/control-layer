-- Add unique constraint on api_keys (user_id, name) with backwards compatibility
-- This migration deletes duplicate API key names per user, keeping the oldest one

-- Step 1: Delete duplicate API keys per user, keeping the oldest one
-- We use a CTE to identify duplicates and delete all but the first (oldest) record
DELETE FROM api_keys
WHERE id IN (
    SELECT id FROM (
        SELECT id,
               ROW_NUMBER() OVER (PARTITION BY user_id, name ORDER BY created_at ASC) as row_num
        FROM api_keys
    ) duplicates
    WHERE row_num > 1
);

-- Step 2: Add the unique constraint now that duplicates are resolved
ALTER TABLE api_keys
ADD CONSTRAINT api_keys_user_id_name_unique UNIQUE (user_id, name);

-- Step 3: Add a comment for documentation
COMMENT ON CONSTRAINT api_keys_user_id_name_unique ON api_keys IS
'Ensures API key names are unique per user';
