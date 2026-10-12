-- Per-account scheduling toleration preset.
--
-- An operator can pin an account to a named scheduling-toleration preset
-- declared in the deployment's config (`scheduling.toleration_presets`). When
-- an account is pinned, every inference request from it carries that preset's
-- toleration list at `nvext.routing_constraints.tolerations`, for realtime,
-- flex and batch traffic alike, so a backend that honours tolerations keeps
-- the account's work on the capacity the operator intends. A typical preset
-- is the empty list, which asks a backend to keep the account off any
-- tainted -- e.g. overflow -- capacity.
--
-- The account stores only the preset's *id*: the tolerations themselves live
-- in config, so an operator can retune a preset without walking every account.
-- NULL means the account is on the "Default" setting and nothing is pinned, so
-- the backend decides. An id that a later config no longer defines is a
-- misconfiguration: the request path logs it and fails closed by applying the
-- empty list (the most restrictive option).
--
-- The column lives on `users` because an account *is* a row in `users`
-- (organizations are `user_type = 'organization'`), alongside the other
-- account-wide flags (`zero_data_retention`, `disabled_modalities`).
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS toleration_preset TEXT;

COMMENT ON COLUMN users.toleration_preset IS
    'Scheduling-toleration preset id pinned to every inference request from this account. NULL = Default (nothing pinned, the backend decides). Resolved against scheduling.toleration_presets in config; an unknown id fails closed to the empty list.';

-- The preset is resolved from the in-memory per-key policy map that the
-- auth_config_changed listener keeps fresh (the same map ZDR and disabled
-- modalities use), so a change must notify like those columns do. Scoped to
-- this one column so unrelated user edits do not trigger a reload. Mirrors
-- users_disabled_modalities_notify (156).
CREATE TRIGGER users_toleration_preset_notify
    AFTER UPDATE OF toleration_preset ON users
    FOR EACH ROW
    WHEN (OLD.toleration_preset IS DISTINCT FROM NEW.toleration_preset)
    EXECUTE FUNCTION notify_config_change();
