-- Make API key names unique per creator instead of per user.
-- For org keys, user_id is the org but different members (created_by) should
-- be able to use the same key name.  For individual users created_by = user_id,
-- so behavior is unchanged.

-- Drop the old partial unique index (created in migration 074)
DROP INDEX api_keys_user_id_name_unique;

-- Add the new partial unique index including created_by
CREATE UNIQUE INDEX api_keys_user_id_created_by_name_unique
  ON api_keys(user_id, created_by, name) WHERE is_deleted = false;
