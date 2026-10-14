-- Account-level realtime priority bias.
--
-- With deadline-ordered realtime priority enabled
-- (`scheduling.inject_realtime_priority`), a realtime request to a dynamo
-- frontend carries `nvext.agent_hints.priority = -T`, with T its effective time
-- in Unix seconds. This account setting moves T earlier by this many seconds on
-- every model the account calls: T = arrival - priority_bias_seconds. A
-- per-model overlay (`model_overlays.priority_bias_seconds`) overrides it on
-- that model, the same precedence `self_hosted_only` uses. Set by platform
-- managers through the account settings API and the console.
--
-- NULL = no bias. Bounded to one day, like the overlay field.
--
-- Additive: a nullable column with no default is a metadata-only change, and the
-- previous release names its `users` columns explicitly. The CHECK is added
-- NOT VALID so no scan of `users` runs under this migration's lock; every
-- existing row is NULL, and the next migration validates it.

-- Adding a column takes an ACCESS EXCLUSIVE lock on a hot table; bound the
-- wait, as the other account-setting migrations do.
SET LOCAL lock_timeout = '5s';

ALTER TABLE users
    ADD COLUMN IF NOT EXISTS priority_bias_seconds INTEGER;

ALTER TABLE users
    ADD CONSTRAINT chk_users_priority_bias_seconds
    CHECK (priority_bias_seconds IS NULL OR priority_bias_seconds BETWEEN 0 AND 86400) NOT VALID;

COMMENT ON COLUMN users.priority_bias_seconds IS
  'Account setting: seconds this account''s realtime requests are moved earlier in the deadline-ordered dynamo priority (T = arrival - bias), on every model unless a model overlay sets its own. NULL = no bias. Applied only when scheduling.inject_realtime_priority is on. 0..86400.';

-- The bias reaches onwards through the account serving settings it syncs on
-- config change, so a change must notify like the other serving settings.
-- Scoped to this column so unrelated user edits do not trigger a reload.
CREATE TRIGGER users_priority_bias_notify
    AFTER UPDATE OF priority_bias_seconds ON users
    FOR EACH ROW
    WHEN (OLD.priority_bias_seconds IS DISTINCT FROM NEW.priority_bias_seconds)
    EXECUTE FUNCTION notify_config_change();
