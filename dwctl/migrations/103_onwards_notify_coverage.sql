-- Close four coverage gaps in the onwards `auth_config_changed` NOTIFY triggers
-- that migrations 102 and 099 left open. All payloads keep 102's unified colon format:
--
--     <table>:<op>:<scope_id>:<epoch_micros>
--
--   <table>        TG_TABLE_NAME
--   <op>           TG_OP (INSERT / UPDATE / DELETE)
--   <scope_id>     a UUID (as text) telling onwards WHAT to reload — a deployment
--                  id (re-query just that deployment) or a user id (re-query every
--                  deployment that user can reach). Over-scoping is safe;
--                  under-scoping is not.
--   <epoch_micros> (extract(epoch from clock_timestamp()) * 1000000)::bigint —
--                  microsecond wall-clock used by the cache-sync lag metric.
--
-- The four fixes:
--
-- GAP 1 — inference_endpoints had NO notify trigger at all, yet the onwards sync
--   reads url / api_key / auth headers from it (dwctl/src/sync/onwards_config:
--   `INNER JOIN inference_endpoints ie ON dm.hosted_on = ie.id`). A change to an
--   endpoint's url or credentials therefore never reloaded the cache until the
--   periodic fallback resync. We add a STATEMENT-level INSERT/UPDATE/DELETE
--   trigger. NEW/OLD are not bound at statement level, so we read the transition
--   tables, take the union of affected endpoint ids, and for each emit one notify
--   per *deployment* hosted on that endpoint
--   (`inference_endpoints:<op>:<deployed_model.id>:<epoch>`) so the consumer can
--   scope by deployment. An endpoint with no live deployments emits nothing.
--
-- GAP 2 — model_traffic_rules UPDATE under-scoping. 102's UPDATE trigger
--   referenced only the NEW transition table, so an UPDATE that *moves* a rule to
--   a different deployed_model_id notified the new deployment but never the old
--   one — the old deployment kept a stale routing rule. We recreate the UPDATE
--   trigger (and add a dedicated update function) to reference BOTH transition
--   tables and emit one notify per DISTINCT deployed_model_id across their UNION.
--   The INSERT and DELETE triggers/function from 102 are left untouched.
--
-- GAP 3 — user_organizations UPDATE under-scoping, identical shape: 102's UPDATE
--   trigger referenced only NEW, so an UPDATE that moves a membership to a
--   different user_id never notified the old user. We recreate the UPDATE trigger
--   (and add a dedicated update function) to reference BOTH transition tables and
--   emit one notify per DISTINCT user_id across their UNION. INSERT and DELETE are
--   left untouched.
--
-- GAP 4 — users.verified was left on the legacy `users:<epoch>` payload (no scope
--   id => full reload). The verified flag drives the rate-limit tier of every key
--   the user owns, so we scope it: emit `users:<op>:<user_id>:<epoch>` so the
--   consumer does a per-user delta instead of a full reload.
--
-- Idempotent: CREATE OR REPLACE FUNCTION and DROP TRIGGER IF EXISTS before each
-- CREATE TRIGGER, so this runs cleanly on a fresh DB and on a DB already carrying
-- migration 102's triggers.

-- ---------------------------------------------------------------------------
-- GAP 1: inference_endpoints (STATEMENT-level, INSERT/UPDATE/DELETE).
-- scope = each DEPLOYMENT id hosted on an affected endpoint.
--
-- NEW/OLD are unbound at statement level, so we read the transition tables.
-- Postgres only permits a NEW TABLE on INSERT/UPDATE triggers and an OLD TABLE on
-- DELETE/UPDATE triggers (the trigger DDL itself enforces this), so the three
-- triggers below bind only the transition table(s) valid for their op. The single
-- shared function — exactly like migration 102's notify_traffic_rules_config_change
-- — branches on TG_OP and reads only the transition table that the firing op
-- bound: new_rows on INSERT, old_rows on DELETE. (A reference to a transition
-- table in an unexecuted branch is fine; Postgres resolves transition tables at
-- execution time, not at CREATE TRIGGER time.) For each DISTINCT affected endpoint
-- id we loop over the live deployments hosted on it and emit one notify per
-- deployment. A brand-new endpoint with no deployments, or one whose deployments
-- are all soft-deleted, emits nothing — correct, since nothing the sync serves
-- changed. UPDATE notifies for the UNION of old+new endpoint ids; the id is
-- immutable so they coincide, but the union keeps the path symmetric and safe.
--
-- DELETE note: inference_endpoints -> deployed_models is ON DELETE CASCADE, and
-- this AFTER STATEMENT trigger fires only after the cascade has run, so the
-- `SELECT ... deployed_models WHERE hosted_on = <deleted endpoint>` finds zero
-- rows and the DELETE branch emits nothing here. That is fine: each cascade-
-- deleted deployment already fires migration 102's row-level deployed_models
-- DELETE trigger and emits its own `deployed_models:DELETE:<deployment_id>`
-- notify, so the consumer still drops exactly those deployments. We keep the
-- symmetric DELETE branch (a) to match the requested per-deployment shape and
-- (b) to remain correct if the FK is ever changed away from CASCADE.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_inference_endpoints_config_change() RETURNS trigger AS $$
DECLARE
    endpoint_id UUID;
    target_id UUID;
BEGIN
    IF TG_OP = 'DELETE' THEN
        FOR endpoint_id IN SELECT DISTINCT id FROM old_rows LOOP
            FOR target_id IN
                SELECT DISTINCT dm.id
                FROM deployed_models dm
                WHERE dm.hosted_on = endpoint_id
                  AND dm.deleted = FALSE
            LOOP
                PERFORM pg_notify('auth_config_changed',
                    TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
                    || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
            END LOOP;
        END LOOP;
    ELSIF TG_OP = 'UPDATE' THEN
        -- Both transition tables exist; the endpoint id is immutable so the union
        -- coincides, but taking it keeps the path symmetric and future-proof.
        FOR endpoint_id IN
            SELECT id FROM new_rows
            UNION
            SELECT id FROM old_rows
        LOOP
            FOR target_id IN
                SELECT DISTINCT dm.id
                FROM deployed_models dm
                WHERE dm.hosted_on = endpoint_id
                  AND dm.deleted = FALSE
            LOOP
                PERFORM pg_notify('auth_config_changed',
                    TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
                    || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
            END LOOP;
        END LOOP;
    ELSE
        -- INSERT exposes only new_rows.
        FOR endpoint_id IN SELECT DISTINCT id FROM new_rows LOOP
            FOR target_id IN
                SELECT DISTINCT dm.id
                FROM deployed_models dm
                WHERE dm.hosted_on = endpoint_id
                  AND dm.deleted = FALSE
            LOOP
                PERFORM pg_notify('auth_config_changed',
                    TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
                    || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
            END LOOP;
        END LOOP;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS inference_endpoints_notify ON inference_endpoints;
DROP TRIGGER IF EXISTS inference_endpoints_notify_insert ON inference_endpoints;
DROP TRIGGER IF EXISTS inference_endpoints_notify_update ON inference_endpoints;
DROP TRIGGER IF EXISTS inference_endpoints_notify_delete ON inference_endpoints;
CREATE TRIGGER inference_endpoints_notify_insert
    AFTER INSERT ON inference_endpoints
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_inference_endpoints_config_change();
CREATE TRIGGER inference_endpoints_notify_update
    AFTER UPDATE ON inference_endpoints
    REFERENCING NEW TABLE AS new_rows OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_inference_endpoints_config_change();
CREATE TRIGGER inference_endpoints_notify_delete
    AFTER DELETE ON inference_endpoints
    REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_inference_endpoints_config_change();

-- ---------------------------------------------------------------------------
-- GAP 2: model_traffic_rules UPDATE (STATEMENT-level). Reference BOTH transition
-- tables so a rule whose deployed_model_id moved notifies the OLD deployment as
-- well as the NEW one. scope = deployed_model_id. INSERT/DELETE from migration
-- 102 (notify_traffic_rules_config_change) are intentionally left in place.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_traffic_rules_config_change_update() RETURNS trigger AS $$
DECLARE
    target_id UUID;
BEGIN
    FOR target_id IN
        SELECT deployed_model_id FROM new_rows
        UNION
        SELECT deployed_model_id FROM old_rows
    LOOP
        PERFORM pg_notify('auth_config_changed',
            TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
            || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    END LOOP;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS model_traffic_rules_notify_update ON model_traffic_rules;
CREATE TRIGGER model_traffic_rules_notify_update
    AFTER UPDATE ON model_traffic_rules
    REFERENCING NEW TABLE AS new_rows OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_traffic_rules_config_change_update();

-- ---------------------------------------------------------------------------
-- GAP 3: user_organizations UPDATE (STATEMENT-level). Reference BOTH transition
-- tables so a membership whose user_id moved notifies the OLD user as well as
-- the NEW one. scope = user_id. INSERT/DELETE from migration 102
-- (notify_user_organizations_config_change) are intentionally left in place.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_user_organizations_config_change_update() RETURNS trigger AS $$
DECLARE
    target_id UUID;
BEGIN
    FOR target_id IN
        SELECT user_id FROM new_rows
        UNION
        SELECT user_id FROM old_rows
    LOOP
        PERFORM pg_notify('auth_config_changed',
            TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
            || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    END LOOP;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS user_organizations_notify_update ON user_organizations;
CREATE TRIGGER user_organizations_notify_update
    AFTER UPDATE ON user_organizations
    REFERENCING NEW TABLE AS new_rows OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_user_organizations_config_change_update();

-- ---------------------------------------------------------------------------
-- GAP 4: users.verified — scope the notify by user instead of a full reload.
-- Migration 099's users_verified_notify is row-level (AFTER UPDATE OF verified,
-- WHEN verified changed) and used the legacy notify_config_change() (no scope id
-- => full reload). The verified flag drives the rate-limit tier of every key the
-- user owns, so the change only affects that user's reachable deployments — emit
-- the user id so the consumer scopes it to a delta (users:<op>:<user_id>:<epoch>).
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_users_verified_config_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || TG_OP || ':' || NEW.id::text || ':'
        || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS users_verified_notify ON users;
CREATE TRIGGER users_verified_notify
    AFTER UPDATE OF verified ON users
    FOR EACH ROW
    WHEN (OLD.verified IS DISTINCT FROM NEW.verified)
    EXECUTE FUNCTION notify_users_verified_config_change();
