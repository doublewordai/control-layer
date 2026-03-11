-- Make API key names unique per creator instead of per user.
-- For org keys, user_id is the org but different members (created_by) should
-- be able to use the same key name.  For individual users created_by = user_id,
-- so behavior is unchanged.

-- Drop the old constraint
ALTER TABLE api_keys DROP CONSTRAINT api_keys_user_id_name_unique;

-- Add the new composite constraint
ALTER TABLE api_keys
ADD CONSTRAINT api_keys_user_id_created_by_name_unique UNIQUE (user_id, created_by, name);
