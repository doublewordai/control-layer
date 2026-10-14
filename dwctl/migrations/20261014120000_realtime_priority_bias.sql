-- Realtime priority bias: per organisation and model, and per account.
--
-- With deadline-ordered realtime priority enabled
-- (`scheduling.inject_realtime_priority`), every realtime request sent to a
-- dynamo frontend carries `nvext.agent_hints.priority = -T`, where T is the
-- request's effective time in Unix seconds and the earliest T is served
-- first. Standard realtime uses T = arrival time; batch work keeps its own
-- T = batch deadline. A bias moves an account's realtime requests earlier:
-- T = arrival - priority_bias_seconds.
--
-- Two places declare it, with the same precedence `self_hosted_only` uses:
--   * users.priority_bias_seconds: the account setting, set by platform
--     managers through the account settings API and the console; applies on
--     every model the account calls;
--   * model_overlays.priority_bias_seconds: an organisation's override on one
--     model, declared in the per-organisation catalog files. It replaces the
--     account value on that model; 0 removes the head start there.
-- NULL = no bias (or, on an overlay, inherit the account's). Both are bounded
-- to one day so a typo cannot push an account's realtime work ahead of every
-- batch deadline in the queue.
--
-- Additive: both columns are nullable with no default (a metadata-only
-- change), and the previous release names its columns explicitly.
--
-- Locking. This file runs in one transaction, so every lock it takes is held
-- until it commits. A CHECK that is validated -- written inline on ADD COLUMN
-- or added without NOT VALID -- scans the whole table while holding ACCESS
-- EXCLUSIVE, even though every row in a freshly added column is NULL. That is
-- harmless on the small model_overlays table, so its CHECK is inline. On
-- `users`, a hot table, the CHECK is added NOT VALID and deliberately never
-- validated: NOT VALID skips the scan, Postgres still enforces the constraint
-- on every insert and update, and the only pre-existing values are the NULLs
-- this migration just created, which satisfy it. (Validating in this same
-- transaction would scan under the ADD COLUMN lock and defeat the purpose.)
SET LOCAL lock_timeout = '5s';

-- ---------------------------------------------------------------------------
-- Per-organisation, per-model override (small table).
ALTER TABLE model_overlays
    ADD COLUMN IF NOT EXISTS priority_bias_seconds INTEGER
    CONSTRAINT chk_model_overlays_priority_bias_seconds
    CHECK (priority_bias_seconds IS NULL OR priority_bias_seconds BETWEEN 0 AND 86400);

COMMENT ON COLUMN model_overlays.priority_bias_seconds IS
  'Seconds this organisation''s realtime requests on this model are moved earlier in the deadline-ordered dynamo priority (T = arrival - bias), replacing the account''s users.priority_bias_seconds on this model. NULL = inherit the account value. Applied only when scheduling.inject_realtime_priority is on. 0..86400.';

-- The existing model_overlays_notify trigger fires on every row change, so a
-- changed bias reaches onwards through the usual config sync.

-- ---------------------------------------------------------------------------
-- Account setting (hot table: no scan, see above).
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
