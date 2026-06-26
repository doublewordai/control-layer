-- Enrich every `auth_config_changed` NOTIFY payload with an operation and a
-- *scope id* so the onwards sync can do a delta reload instead of a full
-- routing-table rebuild.
--
-- Unified payload format (colon-delimited):
--
--     <table>:<op>:<scope_id>:<epoch_micros>
--
--   <table>        TG_TABLE_NAME (e.g. deployed_models)
--   <op>           TG_OP (INSERT / UPDATE / DELETE)
--   <scope_id>     a UUID, rendered as text, that tells onwards WHAT to reload.
--                  Its meaning depends on the table (see per-table mapping
--                  below). The consumer (dwctl/src/sync/onwards_config/mod.rs,
--                  resolve_change_scope) interprets it as either a deployment id
--                  (re-query just that deployment) or a user id (re-query every
--                  deployment that user can reach). Over-scoping is safe;
--                  under-scoping is not.
--   <epoch_micros> (extract(epoch from clock_timestamp()) * 1000000)::bigint —
--                  microsecond wall-clock used by the cache-sync lag metric.
--
-- scope_id per table:
--   deployed_models            -> deployment id  = COALESCE(NEW.id, OLD.id)
--   deployment_groups          -> deployment id  = COALESCE(NEW.deployment_id, OLD.deployment_id)
--   model_tariffs              -> deployment id  = COALESCE(NEW.deployed_model_id, OLD.deployed_model_id)
--   model_traffic_rules        -> deployment id  = COALESCE(NEW.deployed_model_id, OLD.deployed_model_id)
--   deployed_model_components  -> deployment id  = COALESCE(NEW.composite_model_id, OLD.composite_model_id)
--   inference_endpoints        -> deployment id  = each live deployed_model hosted on the endpoint
--   api_keys                   -> user id        = user_id (the key owner)
--   user_groups                -> user id        = COALESCE(NEW.user_id, OLD.user_id)
--   user_organizations         -> user id        = COALESCE(NEW.user_id, OLD.user_id)
--   users (verified)           -> user id        = NEW.id
--
-- <op> values: trigger-emitted notifies carry the SQL operation (INSERT / UPDATE /
-- DELETE). The credits_transactions notify is emitted by the application (not a
-- trigger) and carries a balance-crossing verb instead:
--   * deplete -- a user's balance was exhausted        (request_logging/batcher.rs)
--   * restore -- a user's balance crossed zero upward  (db/handlers/credits.rs)
--
-- DESIGN: the old shared notify_config_change() served tables with DIFFERENT scope
-- columns AND different trigger levels (row vs statement). A single NEW/OLD-reading
-- function cannot be shared by statement-level triggers, because NEW/OLD are not
-- bound for statement-level firing. We therefore use *per-table* trigger functions:
--   * row-level tables read the changed row's column directly (COALESCE(NEW.col,
--     OLD.col), or NEW.id for the UPDATE-only users.verified trigger);
--   * statement-level tables (model_traffic_rules, user_organizations, api_keys,
--     inference_endpoints) read their transition tables and emit one notify per
--     DISTINCT scope id. Their UPDATE path reads BOTH transition tables, so a row
--     whose scope id MOVED notifies the old scope as well as the new.
-- The legacy 2-part `<table>:<epoch_micros>` helper notify_config_change() (migration
-- 049) is kept defined for backward compatibility but is no longer used by any
-- onwards trigger (users.verified now emits the enriched per-user form above).
--
-- This migration is idempotent-friendly: it CREATE OR REPLACEs functions and
-- DROP TRIGGER IF EXISTS / CREATE TRIGGERs, so it runs cleanly on a fresh database
-- and on one already carrying earlier onwards triggers (002/049/067/074/101/042/054/099).

-- ---------------------------------------------------------------------------
-- Shared helper retained for `users` only: legacy 2-part `<table>:<epoch>`
-- payload (no scope id => full reload). Behaviour-preserving for migration 099's
-- users_verified_notify, which is row-level with its own AFTER UPDATE OF verified
-- / WHEN (OLD.verified IS DISTINCT FROM NEW.verified) gating (left untouched).
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_config_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- ---------------------------------------------------------------------------
-- deployed_models (row-level, INSERT/UPDATE/DELETE). scope = the row id itself.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_deployed_models_config_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || TG_OP || ':'
        || COALESCE(NEW.id, OLD.id)::text || ':'
        || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS deployed_models_notify ON deployed_models;
CREATE TRIGGER deployed_models_notify
    AFTER INSERT OR UPDATE OR DELETE ON deployed_models
    FOR EACH ROW EXECUTE FUNCTION notify_deployed_models_config_change();

-- ---------------------------------------------------------------------------
-- deployment_groups (row-level, INSERT/DELETE only — preserves migration 002's
-- events). scope = deployment_id.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_deployment_groups_config_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || TG_OP || ':'
        || COALESCE(NEW.deployment_id, OLD.deployment_id)::text || ':'
        || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS deployment_groups_notify ON deployment_groups;
CREATE TRIGGER deployment_groups_notify
    AFTER INSERT OR DELETE ON deployment_groups
    FOR EACH ROW EXECUTE FUNCTION notify_deployment_groups_config_change();

-- ---------------------------------------------------------------------------
-- user_groups (row-level, INSERT/DELETE only — preserves migration 002's
-- events). scope = user_id.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_user_groups_config_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || TG_OP || ':'
        || COALESCE(NEW.user_id, OLD.user_id)::text || ':'
        || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS user_groups_notify ON user_groups;
CREATE TRIGGER user_groups_notify
    AFTER INSERT OR DELETE ON user_groups
    FOR EACH ROW EXECUTE FUNCTION notify_user_groups_config_change();

-- ---------------------------------------------------------------------------
-- model_tariffs (row-level, INSERT/UPDATE/DELETE). Migration 042 emitted a JSON
-- payload keyed by the TARIFF row id. Switch to the colon format AND change the
-- scope id to the DEPLOYMENT id (deployed_model_id) so onwards reloads the
-- affected deployment, not a non-existent "tariff" entity.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_onwards_config_on_tariff_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || TG_OP || ':'
        || COALESCE(NEW.deployed_model_id, OLD.deployed_model_id)::text || ':'
        || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS model_tariffs_notify_onwards ON model_tariffs;
CREATE TRIGGER model_tariffs_notify_onwards
    AFTER INSERT OR UPDATE OR DELETE ON model_tariffs
    FOR EACH ROW EXECUTE FUNCTION notify_onwards_config_on_tariff_change();

-- ---------------------------------------------------------------------------
-- deployed_model_components (row-level, INSERT/UPDATE/DELETE). Migration 054
-- emitted a JSON payload keyed by the COMPONENT row id. Switch to the colon
-- format AND change the scope id to the composite DEPLOYMENT id
-- (composite_model_id) so onwards reloads the composite deployment.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_onwards_config_on_component_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || TG_OP || ':'
        || COALESCE(NEW.composite_model_id, OLD.composite_model_id)::text || ':'
        || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS deployed_model_components_notify_onwards ON deployed_model_components;
CREATE TRIGGER deployed_model_components_notify_onwards
    AFTER INSERT OR UPDATE OR DELETE ON deployed_model_components
    FOR EACH ROW EXECUTE FUNCTION notify_onwards_config_on_component_change();

-- ---------------------------------------------------------------------------
-- model_traffic_rules (STATEMENT-level, INSERT/UPDATE/DELETE — preserves
-- migration 067's level). NEW/OLD are not bound at statement level, so we read
-- the transition tables and emit one notify per DISTINCT deployed_model_id.
-- A statement may touch rules for several models (or none — DO NOTHING / no-op
-- UPDATE — in which case the transition table is empty and we notify nothing).
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_traffic_rules_config_change() RETURNS trigger AS $$
DECLARE
    target_id UUID;
BEGIN
    IF TG_OP = 'DELETE' THEN
        FOR target_id IN SELECT DISTINCT deployed_model_id FROM old_rows LOOP
            PERFORM pg_notify('auth_config_changed',
                TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
                || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
        END LOOP;
    ELSE
        -- INSERT and UPDATE both expose the new_rows transition table.
        FOR target_id IN SELECT DISTINCT deployed_model_id FROM new_rows LOOP
            PERFORM pg_notify('auth_config_changed',
                TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
                || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
        END LOOP;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- UPDATE reads BOTH transition tables (via a dedicated function) so a rule whose
-- deployed_model_id MOVED notifies the OLD deployment as well as the new one;
-- INSERT/DELETE keep the base function above.
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

DROP TRIGGER IF EXISTS model_traffic_rules_notify ON model_traffic_rules;
DROP TRIGGER IF EXISTS model_traffic_rules_notify_insert ON model_traffic_rules;
DROP TRIGGER IF EXISTS model_traffic_rules_notify_update ON model_traffic_rules;
DROP TRIGGER IF EXISTS model_traffic_rules_notify_delete ON model_traffic_rules;
CREATE TRIGGER model_traffic_rules_notify_insert
    AFTER INSERT ON model_traffic_rules
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_traffic_rules_config_change();
CREATE TRIGGER model_traffic_rules_notify_update
    AFTER UPDATE ON model_traffic_rules
    REFERENCING NEW TABLE AS new_rows OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_traffic_rules_config_change_update();
CREATE TRIGGER model_traffic_rules_notify_delete
    AFTER DELETE ON model_traffic_rules
    REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_traffic_rules_config_change();

-- ---------------------------------------------------------------------------
-- user_organizations (STATEMENT-level, INSERT/UPDATE/DELETE — preserves
-- migration 074's level). scope = user_id. Read transition tables; emit one
-- notify per DISTINCT user_id. (The separate BEFORE INSERT/UPDATE row-level
-- enforce_organization_membership_types trigger from migration 074 is left
-- untouched.)
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_user_organizations_config_change() RETURNS trigger AS $$
DECLARE
    target_id UUID;
BEGIN
    IF TG_OP = 'DELETE' THEN
        FOR target_id IN SELECT DISTINCT user_id FROM old_rows LOOP
            PERFORM pg_notify('auth_config_changed',
                TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
                || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
        END LOOP;
    ELSE
        FOR target_id IN SELECT DISTINCT user_id FROM new_rows LOOP
            PERFORM pg_notify('auth_config_changed',
                TG_TABLE_NAME || ':' || TG_OP || ':' || target_id::text || ':'
                || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
        END LOOP;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- UPDATE reads BOTH transition tables (via a dedicated function) so a membership
-- whose user_id MOVED notifies the OLD user as well as the new one; INSERT/DELETE
-- keep the base function above.
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

DROP TRIGGER IF EXISTS user_organizations_notify ON user_organizations;
DROP TRIGGER IF EXISTS user_organizations_notify_insert ON user_organizations;
DROP TRIGGER IF EXISTS user_organizations_notify_update ON user_organizations;
DROP TRIGGER IF EXISTS user_organizations_notify_delete ON user_organizations;
CREATE TRIGGER user_organizations_notify_insert
    AFTER INSERT ON user_organizations
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_user_organizations_config_change();
CREATE TRIGGER user_organizations_notify_update
    AFTER UPDATE ON user_organizations
    REFERENCING NEW TABLE AS new_rows OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_user_organizations_config_change_update();
CREATE TRIGGER user_organizations_notify_delete
    AFTER DELETE ON user_organizations
    REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_user_organizations_config_change();

-- ---------------------------------------------------------------------------
-- api_keys (STATEMENT-level over transition tables — preserves migration 101).
-- scope = user_id (the key owner). The firing CONDITIONS from 101 are preserved
-- EXACTLY:
--   * INSERT -- only when at least one row was actually inserted (a DO NOTHING
--               no-op upsert leaves new_rows empty -> no notify),
--   * DELETE -- only when at least one row was deleted,
--   * UPDATE -- only when at least one row changed a column the onwards sync
--               consumes (secret, purpose, user_id, requests_per_second,
--               burst_size, is_deleted, hidden); metadata-only updates (name,
--               description, last_used) must NOT reload the cache.
-- The ONLY change vs 101 is the payload: instead of a single
-- `api_keys:<epoch>` notification, we now emit one
-- `api_keys:<op>:<user_id>:<epoch>` per DISTINCT affected user_id, so onwards
-- can scope the reload to that user's deployments. The relevance gate still
-- runs first, so non-consumed changes emit nothing at all.
--
-- For UPDATE we restrict the DISTINCT user_id set to the keys that actually
-- changed a consumed column (the same predicate as the 101 relevance check),
-- joining new_rows to old_rows on the immutable primary key. A key whose user_id
-- itself changed (ownership move) affects both the old and the new owner, so we
-- notify both.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION notify_api_keys_config_change() RETURNS trigger AS $$
DECLARE
    relevant_change boolean := false;
    target_id UUID;
BEGIN
    IF TG_OP = 'INSERT' THEN
        relevant_change := EXISTS (SELECT 1 FROM new_rows);
        IF relevant_change THEN
            FOR target_id IN SELECT DISTINCT user_id FROM new_rows LOOP
                PERFORM pg_notify('auth_config_changed',
                    'api_keys:' || TG_OP || ':' || target_id::text || ':'
                    || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
            END LOOP;
        END IF;
    ELSIF TG_OP = 'DELETE' THEN
        relevant_change := EXISTS (SELECT 1 FROM old_rows);
        IF relevant_change THEN
            FOR target_id IN SELECT DISTINCT user_id FROM old_rows LOOP
                PERFORM pg_notify('auth_config_changed',
                    'api_keys:' || TG_OP || ':' || target_id::text || ':'
                    || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
            END LOOP;
        END IF;
    ELSIF TG_OP = 'UPDATE' THEN
        -- Only columns the sync query reads matter; metadata-only updates
        -- (name, description, last_used) must not reload the cache. Joined on the
        -- immutable primary key. This is the exact predicate from migration 101;
        -- here we also collect the affected user_id(s) of the changed keys
        -- (both old and new owner, in case ownership moved).
        FOR target_id IN
            SELECT user_id FROM (
                SELECT o.user_id
                FROM new_rows n
                JOIN old_rows o ON o.id = n.id
                WHERE o.secret              IS DISTINCT FROM n.secret
                   OR o.purpose             IS DISTINCT FROM n.purpose
                   OR o.user_id             IS DISTINCT FROM n.user_id
                   OR o.requests_per_second IS DISTINCT FROM n.requests_per_second
                   OR o.burst_size          IS DISTINCT FROM n.burst_size
                   OR o.is_deleted          IS DISTINCT FROM n.is_deleted
                   OR o.hidden              IS DISTINCT FROM n.hidden
                UNION
                SELECT n.user_id
                FROM new_rows n
                JOIN old_rows o ON o.id = n.id
                WHERE o.secret              IS DISTINCT FROM n.secret
                   OR o.purpose             IS DISTINCT FROM n.purpose
                   OR o.user_id             IS DISTINCT FROM n.user_id
                   OR o.requests_per_second IS DISTINCT FROM n.requests_per_second
                   OR o.burst_size          IS DISTINCT FROM n.burst_size
                   OR o.is_deleted          IS DISTINCT FROM n.is_deleted
                   OR o.hidden              IS DISTINCT FROM n.hidden
            ) affected
            GROUP BY user_id
        LOOP
            PERFORM pg_notify('auth_config_changed',
                'api_keys:' || TG_OP || ':' || target_id::text || ':'
                || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
        END LOOP;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- Triggers unchanged from migration 101 (same level, events, transition tables);
-- recreated so a fresh DB and an already-migrated DB converge identically.
DROP TRIGGER IF EXISTS api_keys_notify_insert ON api_keys;
DROP TRIGGER IF EXISTS api_keys_notify_delete ON api_keys;
DROP TRIGGER IF EXISTS api_keys_notify_update ON api_keys;

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

-- ---------------------------------------------------------------------------
-- inference_endpoints (STATEMENT-level, INSERT/UPDATE/DELETE). The onwards sync
-- reads url / api_key / auth headers from it (`INNER JOIN inference_endpoints ie
-- ON dm.hosted_on = ie.id`), so an endpoint change must reload the deployments
-- hosted on it. scope = each DEPLOYMENT id hosted on an affected endpoint. NEW/OLD
-- are unbound at statement level, so the shared function branches on TG_OP and
-- reads the bound transition table (new_rows on INSERT, old_rows on DELETE, both on
-- UPDATE), then for each affected endpoint id emits one notify per live deployment
-- hosted on it. An endpoint with no live deployments emits nothing.
--
-- DELETE note: inference_endpoints -> deployed_models is ON DELETE CASCADE and this
-- AFTER STATEMENT trigger fires after the cascade, so the DELETE branch finds zero
-- hosted deployments and emits nothing here — fine, because each cascade-deleted
-- deployment already fires the row-level deployed_models DELETE trigger above. The
-- symmetric DELETE branch is kept to match the per-deployment shape and to stay
-- correct if the FK is ever changed away from CASCADE.
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
-- users.verified (row-level, AFTER UPDATE OF verified, gated on the flag actually
-- changing). The verified flag drives the rate-limit tier of every key the user
-- owns, so a change only affects that user's reachable deployments — emit the user
-- id (users:<op>:<user_id>:<epoch>) so the consumer does a per-user delta. This
-- replaces migration 099's legacy users_verified_notify (which used the 2-part
-- notify_config_change() / full-reload form).
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
