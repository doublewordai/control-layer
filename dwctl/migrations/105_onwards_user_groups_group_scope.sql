-- Re-scope the user_groups `auth_config_changed` NOTIFY from the USER id to the
-- GROUP id, so a membership change becomes a cheap scoped delta that still
-- captures revocations.
--
-- Payload keeps migration 103's unified colon format:
--
--     <table>:<op>:<scope_id>:<epoch_micros>
--
-- WHY THE CHANGE: a user_groups INSERT/DELETE means a user joined or left a
-- group G. That can only change the key list of the deployments G grants
-- (deployment_groups WHERE group_id = G). Migration 103 emitted the USER id here,
-- which the consumer resolved via the user's CURRENT reachable deployments — but
-- on a REVOCATION the affected deployment is one the user can NO LONGER reach, so
-- a user-scoped delta misses it and leaves the revoked key cached. The consumer
-- therefore had to fall back to a full reload for user_groups. Emitting the GROUP
-- id instead lets the consumer resolve the group's deployments directly
-- (resolve_change_scope -> deployments_for_group), a set that always INCLUDES the
-- just-revoked deployment, so the change stays a cheap delta AND the revoked
-- deployment's key list is refreshed. Over-scoping is safe; under-scoping is not,
-- and the group's deployment set is exactly the affected set.
--
-- The consumer (dwctl/src/sync/onwards_config/mod.rs, resolve_change_scope) now
-- interprets the user_groups scope id as a GROUP id rather than a user id.
--
-- Trigger level and events are unchanged from migration 103 (row-level AFTER
-- INSERT OR DELETE); only the scope column in the payload changes. Idempotent:
-- CREATE OR REPLACE FUNCTION + DROP TRIGGER IF EXISTS / CREATE TRIGGER, so it runs
-- cleanly on a fresh DB and on one already carrying migration 103's trigger.
--
-- NOTE: user_organizations is intentionally left on the user-id scope (full
-- reload in the consumer). Organization membership does not grant deployment
-- access — the routing query joins user_groups + the public group only and never
-- references user_organizations — so there is nothing to scope, and the rare full
-- reload is harmless.

CREATE OR REPLACE FUNCTION notify_user_groups_config_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || TG_OP || ':'
        || COALESCE(NEW.group_id, OLD.group_id)::text || ':'
        || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS user_groups_notify ON user_groups;
CREATE TRIGGER user_groups_notify
    AFTER INSERT OR DELETE ON user_groups
    FOR EACH ROW EXECUTE FUNCTION notify_user_groups_config_change();
