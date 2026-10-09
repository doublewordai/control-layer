-- Per-account pinned scheduling tolerations.
--
-- An operator can affix a fixed list of scheduling tolerations to every
-- inference request from a given account. The list is written verbatim to
-- `nvext.routing_constraints.tolerations` on the request body before it
-- reaches the upstream inference backend, for both realtime and batch
-- traffic. A typical value is the empty list, which asks a backend that
-- honours tolerations to keep the account's work off any tainted capacity.
--
-- NULL means "not pinned": the account carries no tolerations of its own and
-- is unchanged. An empty list (`'[]'::jsonb`) is a real pin, distinct from
-- NULL. The column lives on `users` because an account *is* a row in `users`
-- (organizations are `user_type = 'organization'`), alongside the other
-- account-wide flags (`zero_data_retention`, `disabled_modalities`).
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS pinned_tolerations JSONB;

COMMENT ON COLUMN users.pinned_tolerations IS
    'Scheduling tolerations pinned to every inference request from this account, written to nvext.routing_constraints.tolerations. NULL = not pinned; a JSON array is the pinned list ([] forbids tainted capacity).';

-- Refuse a non-array shape (apart from NULL) so a direct SQL edit cannot
-- store something the request path would serialise into an invalid body.
ALTER TABLE users
    DROP CONSTRAINT IF EXISTS users_pinned_tolerations_is_array;
ALTER TABLE users
    ADD CONSTRAINT users_pinned_tolerations_is_array
    CHECK (pinned_tolerations IS NULL OR jsonb_typeof(pinned_tolerations) = 'array');

-- The tolerations are answered from the in-memory per-key policy map that the
-- auth_config_changed listener keeps fresh (the same map ZDR and disabled
-- modalities use), so a change must notify like those columns do. Scoped to
-- this one column so unrelated user edits do not trigger a reload. Mirrors
-- users_disabled_modalities_notify (156).
CREATE TRIGGER users_pinned_tolerations_notify
    AFTER UPDATE OF pinned_tolerations ON users
    FOR EACH ROW
    WHEN (OLD.pinned_tolerations IS DISTINCT FROM NEW.pinned_tolerations)
    EXECUTE FUNCTION notify_config_change();
