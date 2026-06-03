-- Scope the api_keys config-change NOTIFY so it fires at most once per statement,
-- and only when something the onwards cache actually consumes has changed.
--
-- db/handlers/api_keys.rs::get_or_create_hidden_key runs
--     INSERT INTO api_keys (...) ON CONFLICT (...) DO NOTHING
-- very frequently; on the common "key already exists" path it writes zero rows,
-- yet the original FOR EACH STATEMENT trigger fired an AFTER INSERT notification
-- on every call (statement-level triggers fire even when no rows change). Each
-- notification drives a full routing-table reload in onwards, so these no-op
-- upserts were a dominant source of redundant config reloads (and DB egress).
--
-- Fix: statement-level triggers with transition tables, notifying only when:
--   * INSERT -- at least one row was actually inserted (a DO NOTHING no-op leaves
--              the NEW transition table empty -> no notify),
--   * DELETE -- at least one row was deleted,
--   * UPDATE -- at least one row changed a column onwards consumes.
-- A statement-level trigger fires once per statement, so this also avoids the
-- per-row notification storm a FOR EACH ROW trigger would cause on bulk
-- operations (e.g. soft_delete_member_org_keys updates all of a user's keys).

DROP TRIGGER IF EXISTS api_keys_notify ON api_keys;

CREATE OR REPLACE FUNCTION notify_api_keys_config_change() RETURNS trigger AS $$
DECLARE
    relevant_change boolean := false;
BEGIN
    IF TG_OP = 'INSERT' THEN
        relevant_change := EXISTS (SELECT 1 FROM new_rows);
    ELSIF TG_OP = 'DELETE' THEN
        relevant_change := EXISTS (SELECT 1 FROM old_rows);
    ELSIF TG_OP = 'UPDATE' THEN
        -- Only columns the sync query reads matter; metadata-only updates
        -- (name, description, last_used) must not reload the cache. Joined on the
        -- immutable primary key.
        relevant_change := EXISTS (
            SELECT 1
            FROM new_rows n
            JOIN old_rows o ON o.id = n.id
            WHERE o.secret              IS DISTINCT FROM n.secret
               OR o.purpose             IS DISTINCT FROM n.purpose
               OR o.user_id             IS DISTINCT FROM n.user_id
               OR o.requests_per_second IS DISTINCT FROM n.requests_per_second
               OR o.burst_size          IS DISTINCT FROM n.burst_size
               OR o.is_deleted          IS DISTINCT FROM n.is_deleted
               OR o.hidden              IS DISTINCT FROM n.hidden
        );
    END IF;

    IF relevant_change THEN
        -- Match notify_config_change()'s payload format (migration 049) so the
        -- cache-sync lag metric keeps attributing reloads to the api_keys table.
        PERFORM pg_notify('auth_config_changed',
            'api_keys:' || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER api_keys_notify_insert
    AFTER INSERT ON api_keys
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_api_keys_config_change();

CREATE TRIGGER api_keys_notify_delete
    AFTER DELETE ON api_keys
    REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_api_keys_config_change();

CREATE TRIGGER api_keys_notify_update
    AFTER UPDATE ON api_keys
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_api_keys_config_change();
