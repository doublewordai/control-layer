-- Per-organization disabled modalities.
--
-- An organization owner can switch off a whole product surface for their
-- workspace: `realtime` (the synchronous inference endpoints under /ai/v1,
-- whatever service tier the request asks for) and/or `batch` (the Files and
-- Batches API). The block is enforced at the endpoint, for every API key the
-- organization owns, so a member cannot route around it by minting a new key
-- or by using a different key purpose.
--
-- Lives on `users` because organizations *are* rows in `users`
-- (`user_type = 'organization'`), alongside the other org-wide account flags
-- (`zero_data_retention`, `auto_join_enabled`). Empty for every existing row:
-- nothing changes for anyone until an owner turns something off. Personal
-- accounts keep the empty default; the API only writes it for organizations.
--
-- Owner-only in the API, same gate as `zero_data_retention`: an admin can run
-- the workspace's day to day but may not decide which products it uses.
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS disabled_modalities TEXT[] NOT NULL DEFAULT '{}';

COMMENT ON COLUMN users.disabled_modalities IS
    'Organizations only: product surfaces an owner has switched off for every key the organization owns. Values: realtime (POST inference endpoints under /ai/v1), batch (Files and Batches API). Empty means everything is allowed.';

-- Refuse anything the enforcement code would not understand, so a typo in a
-- direct SQL edit cannot silently allow or deny a surface.
ALTER TABLE users
    ADD CONSTRAINT users_disabled_modalities_known
    CHECK (disabled_modalities <@ ARRAY['realtime', 'batch']::TEXT[]);

-- The realtime block is answered from the in-memory per-key policy map that
-- the auth_config_changed listener keeps fresh (the same map ZDR uses), so a
-- change must notify like the ZDR flag does. Scoped to this one column so
-- unrelated user edits do not trigger a reload. Mirrors
-- users_zero_data_retention_notify (109).
CREATE TRIGGER users_disabled_modalities_notify
    AFTER UPDATE OF disabled_modalities ON users
    FOR EACH ROW
    WHEN (OLD.disabled_modalities IS DISTINCT FROM NEW.disabled_modalities)
    EXECUTE FUNCTION notify_config_change();
