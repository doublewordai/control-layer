-- Add a `zero_data_retention` (ZDR) flag to users, toggled by admins from the
-- user/organization access pages. It is an account-wide setting: it lives on
-- the users row and is read by the onwards sync when building per-key config,
-- so every API key owned by that account inherits it.
--
-- For organizations (users.user_type = 'organization'), org-context API keys
-- carry user_id = the org id, so enabling ZDR on the org enables it for every
-- key created within that org. A member's personal keys are governed by the
-- member's own flag. This mirrors how `verified` (migration 099) tiers org
-- keys by the org and personal keys by the individual.
--
-- Scope note: this migration only stores the flag and surfaces it to onwards
-- (as a "zdr" key label). onwards does not act on the label yet.
ALTER TABLE users
    ADD COLUMN zero_data_retention BOOLEAN NOT NULL DEFAULT false;

-- Notify the onwards sync when the flag flips so the in-memory cache picks up
-- the new label immediately, without waiting for an unrelated config change.
-- Scoped to UPDATEs of this one column (via the WHEN clause) so unrelated user
-- edits do not trigger a sync rebuild. Mirrors users_verified_notify (099).
CREATE TRIGGER users_zero_data_retention_notify
    AFTER UPDATE OF zero_data_retention ON users
    FOR EACH ROW
    WHEN (OLD.zero_data_retention IS DISTINCT FROM NEW.zero_data_retention)
    EXECUTE FUNCTION notify_config_change();
