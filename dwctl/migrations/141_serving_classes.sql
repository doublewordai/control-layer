-- Serving classes v1.
--
-- A serving class is the dispatch mode a request is served under:
-- `interactive` and `throughput` are elevated classes a model activates and
-- an organisation is granted; `standard` is the absence of a choice and is
-- never stored. Onwards resolves the class per request (suffix > key >
-- overlay default > account default), gated by the model's active classes
-- and the org's overlay, and stamps the dynamo pool tag and priority band.
--
-- Three levels of declared state:
--   * the model: which elevated classes are active (`deployed_models.serving_classes`);
--   * the org, per model: an OVERLAY (`model_overlays`) — granted classes, a
--     default class for that model, and a per-model override of the account's
--     routing preference. Declared in the model catalog, materialised here;
--   * the org, for every model: ACCOUNT SETTINGS on the users row, next to
--     zero_data_retention — a default class and "never fall over to an
--     external provider".
-- Plus the key: an optional class a specific API key requests by default.

-- ---------------------------------------------------------------------------
-- The model: active elevated classes.
ALTER TABLE deployed_models
    ADD COLUMN serving_classes TEXT[] NOT NULL DEFAULT '{}';

ALTER TABLE deployed_models
    ADD CONSTRAINT chk_deployed_models_serving_classes
    CHECK (serving_classes <@ ARRAY['interactive', 'throughput']::text[]);

COMMENT ON COLUMN deployed_models.serving_classes IS
  'Elevated serving classes this model has activated (subset of interactive, throughput). Empty = standard only; a requested elevated class resolves to standard.';

-- ---------------------------------------------------------------------------
-- The key: a class this key requests by default (still gated by the org's
-- grant and the model's active classes at resolution time).
ALTER TABLE api_keys
    ADD COLUMN serving_class TEXT;

ALTER TABLE api_keys
    ADD CONSTRAINT chk_api_keys_serving_class
    CHECK (serving_class IS NULL OR serving_class IN ('interactive', 'throughput'));

COMMENT ON COLUMN api_keys.serving_class IS
  'Serving class requested by default for this key (interactive | throughput). NULL = no key-level preference. A request suffix outranks it.';

-- The api_keys NOTIFY is scoped to the columns the onwards sync reads
-- (migration 101); the key class is now one of them.
CREATE OR REPLACE FUNCTION notify_api_keys_config_change() RETURNS trigger AS $$
DECLARE
    relevant_change boolean := false;
BEGIN
    IF TG_OP = 'INSERT' THEN
        relevant_change := EXISTS (SELECT 1 FROM new_rows);
    ELSIF TG_OP = 'DELETE' THEN
        relevant_change := EXISTS (SELECT 1 FROM old_rows);
    ELSIF TG_OP = 'UPDATE' THEN
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
               OR o.serving_class       IS DISTINCT FROM n.serving_class
        );
    END IF;

    IF relevant_change THEN
        PERFORM pg_notify('auth_config_changed',
            'api_keys:' || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- ---------------------------------------------------------------------------
-- Account settings: org-wide, every model the org calls. Same shape and sync
-- path as zero_data_retention (migration 109): read by the onwards sync when
-- building per-key config, so every key owned by the account inherits them.
ALTER TABLE users
    ADD COLUMN default_serving_class TEXT,
    ADD COLUMN self_hosted_only BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE users
    ADD CONSTRAINT chk_users_default_serving_class
    CHECK (default_serving_class IS NULL OR default_serving_class IN ('interactive', 'throughput'));

COMMENT ON COLUMN users.default_serving_class IS
  'Account setting: serving class this account''s realtime requests ask for when neither the request nor the key names one. Still gated by the org''s per-model grant.';
COMMENT ON COLUMN users.self_hosted_only IS
  'Account setting: never fall over to an external (untrusted) provider; onwards restricts the composite to its self-hosted members for this account''s requests. An overlay can override it per model.';

CREATE TRIGGER users_serving_settings_notify
    AFTER UPDATE OF default_serving_class, self_hosted_only ON users
    FOR EACH ROW
    WHEN (OLD.default_serving_class IS DISTINCT FROM NEW.default_serving_class
          OR OLD.self_hosted_only IS DISTINCT FROM NEW.self_hosted_only)
    EXECUTE FUNCTION notify_config_change();

-- ---------------------------------------------------------------------------
-- Overlays: one org's modifiers on one model. Authored in the model catalog
-- (Josh's provisioning YAML, `clay.overlays`), materialised here by the
-- provisioner; the console renders them and never authors them.
CREATE TABLE model_overlays (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    -- Name of the tariff on this model the org is charged under. NULL = the
    -- model's general tariff. Resolved by the tariff assigner in a later step;
    -- stored now so the catalog can declare the whole deal in one place.
    tariff_name TEXT,
    -- Elevated classes the org may use on this model.
    granted_classes TEXT[] NOT NULL DEFAULT '{}',
    -- Class the org's requests to this model ask for when neither the request
    -- nor the key names one. Outranks the account's default_serving_class.
    default_serving_class TEXT,
    -- Per-model override of the account's self_hosted_only. NULL = inherit.
    self_hosted_only BOOLEAN,
    -- Set when the row is owned by the model catalog (same marker format as
    -- deployed_models.provisioning_source); NULL = managed by hand.
    provisioning_source TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT model_overlays_one_per_org_model UNIQUE (user_id, deployed_model_id),
    CONSTRAINT chk_model_overlays_granted_classes
        CHECK (granted_classes <@ ARRAY['interactive', 'throughput']::text[]),
    CONSTRAINT chk_model_overlays_default_serving_class
        CHECK (default_serving_class IS NULL OR default_serving_class IN ('interactive', 'throughput'))
);

COMMENT ON TABLE model_overlays IS
  'One organisation''s modifiers on one model: tariff name, granted serving classes, per-model default class, per-model routing override. Declared in the model catalog; applied by API-key owner, never addressed by name.';

CREATE INDEX idx_model_overlays_deployed_model_id ON model_overlays (deployed_model_id);

CREATE TRIGGER model_overlays_notify
    AFTER INSERT OR UPDATE OR DELETE ON model_overlays
    EXECUTE FUNCTION notify_config_change();

-- ---------------------------------------------------------------------------
-- Analytics: what the request asked for and what it was served as. `resolved`
-- is always set on rows that reached onwards' resolver; `requested` is NULL
-- when nothing named a class. Aggregates key on the resolved class.
ALTER TABLE http_analytics
    ADD COLUMN requested_serving_class TEXT,
    ADD COLUMN resolved_serving_class TEXT;

COMMENT ON COLUMN http_analytics.requested_serving_class IS
  'Serving class the request asked for (suffix > key > overlay default > account default) before entitlement; NULL when nothing named one.';
COMMENT ON COLUMN http_analytics.resolved_serving_class IS
  'Serving class the request was dispatched under after the model''s active classes and the org''s grant were applied (interactive | throughput | standard).';
