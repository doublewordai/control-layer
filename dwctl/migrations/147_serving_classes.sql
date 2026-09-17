-- Serving classes v1.
--
-- A serving class is the dispatch mode a request is served under:
-- `interactive` and `throughput` are elevated classes; `standard` is the
-- absence of a choice and is never stored. Onwards resolves the class per
-- request (request suffix > the org's per-model overlay default > the org's
-- account default), and an elevated class applies only when the ORGANISATION
-- holds it and the MODEL offers it. The resolved class is stamped on the
-- dynamo member as a pool tag and priority band.
--
-- Declared state, by where it lives:
--   * the model: the classes it offers, each a PRESET of objective targets
--     (`deployed_models.serving_classes`: class → {ttft_ms, itl_ms, priority}), declared
--     in the model catalog once its pools exist. Onwards sends the resolved preset to
--     the dynamo frontend as `nvext.router` targets, which the GlobalRouter maps to a
--     pool;
--   * the organisation, org-wide: ACCOUNT SETTINGS on the users row, next to
--     zero_data_retention: the classes it holds, a default class, and "never
--     fall over to an external provider". Operational settings, toggled in the
--     console like ZDR;
--   * the organisation, per model: an OVERLAY (`model_overlays`) — a per-model
--     override of the account default class (or explicit targets for a bespoke
--     deal, which imply authority) or of the routing preference. Declared in the
--     per-organisation catalog files and materialised here;
--   * the endpoint: what kind of server it is (`inference_endpoints.kind`), so
--     the envelope is only ever sent to the dynamo frontend and a "no external
--     provider" restriction knows which members are external.

-- ---------------------------------------------------------------------------
-- The model: the classes it offers, as presets of targets.
ALTER TABLE deployed_models
    ADD COLUMN serving_classes JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE deployed_models
    ADD CONSTRAINT chk_deployed_models_serving_classes
    CHECK (jsonb_typeof(serving_classes) = 'object');

COMMENT ON COLUMN deployed_models.serving_classes IS
  'Serving classes this model offers, keyed by class name (interactive, throughput, optionally standard), each a preset {ttft_ms, itl_ms, priority} declared in the model catalog once its pools exist. Empty object = standard only.';

-- ---------------------------------------------------------------------------
-- Account settings: org-wide, every model the org calls. Same shape and sync
-- path as zero_data_retention (migration 109).
ALTER TABLE users
    ADD COLUMN granted_serving_classes TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN default_serving_class TEXT,
    ADD COLUMN self_hosted_only BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE users
    ADD CONSTRAINT chk_users_granted_serving_classes
    CHECK (granted_serving_classes <@ ARRAY['interactive', 'throughput']::text[]),
    ADD CONSTRAINT chk_users_default_serving_class
    CHECK (default_serving_class IS NULL OR default_serving_class IN ('interactive', 'throughput'));

COMMENT ON COLUMN users.granted_serving_classes IS
  'Account setting: elevated serving classes this organisation holds. An explicit request for a class not held is rejected; a class held applies wherever the model offers it.';
COMMENT ON COLUMN users.default_serving_class IS
  'Account setting: serving class this organisation''s realtime requests ask for when the request names none. Silently resolves to standard on models that do not offer it.';
COMMENT ON COLUMN users.self_hosted_only IS
  'Account setting: never fall over to an external provider; onwards restricts the composite to its non-external members for this organisation''s requests. An overlay can override it per model.';

CREATE TRIGGER users_serving_settings_notify
    AFTER UPDATE OF granted_serving_classes, default_serving_class, self_hosted_only ON users
    FOR EACH ROW
    WHEN (OLD.granted_serving_classes IS DISTINCT FROM NEW.granted_serving_classes
          OR OLD.default_serving_class IS DISTINCT FROM NEW.default_serving_class
          OR OLD.self_hosted_only IS DISTINCT FROM NEW.self_hosted_only)
    EXECUTE FUNCTION notify_config_change();

-- ---------------------------------------------------------------------------
-- Overlays: one organisation's per-model overrides. Authored in the
-- per-organisation catalog files, materialised here by the provisioner; the
-- console renders them and never authors them.
CREATE TABLE model_overlays (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    -- Overrides the account's default_serving_class on this model. NULL = inherit.
    default_serving_class TEXT,
    -- Explicit targets {ttft_ms, itl_ms, priority} for a bespoke deal on this model,
    -- taking precedence over any class preset. Writing them implies authority, so no
    -- class grant is checked. Mutually exclusive with default_serving_class.
    targets JSONB,
    -- Overrides the account's self_hosted_only on this model. NULL = inherit.
    self_hosted_only BOOLEAN,
    -- Set when the row is owned by the catalog (same marker format as
    -- deployed_models.provisioning_source); NULL = managed by hand.
    provisioning_source TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT model_overlays_one_per_org_model UNIQUE (user_id, deployed_model_id),
    CONSTRAINT chk_model_overlays_default_serving_class
        CHECK (default_serving_class IS NULL OR default_serving_class IN ('interactive', 'throughput')),
    CONSTRAINT chk_model_overlays_targets
        CHECK (targets IS NULL OR jsonb_typeof(targets) = 'object'),
    CONSTRAINT chk_model_overlays_class_xor_targets
        CHECK (default_serving_class IS NULL OR targets IS NULL)
);

COMMENT ON TABLE model_overlays IS
  'One organisation''s per-model overrides of its account settings (default serving class or explicit targets, routing preference). Declared in the per-organisation catalog; applied by API-key owner, never addressed by name.';

CREATE INDEX idx_model_overlays_deployed_model_id ON model_overlays (deployed_model_id);

CREATE TRIGGER model_overlays_notify
    AFTER INSERT OR UPDATE OR DELETE ON model_overlays
    EXECUTE FUNCTION notify_config_change();

-- ---------------------------------------------------------------------------
-- The endpoint: what kind of server it is. `dynamo` is the self-hosted fleet
-- behind the dynamo frontend (the only kind that receives the serving-class
-- envelope); `hosted` is self-hosted but not behind dynamo; `external` is a
-- third-party provider. Backfilled from the scheduling-priority capability,
-- which only the dynamo frontend endpoint carries today; independent of it
-- from here on, and set explicitly when an endpoint is created.
ALTER TABLE inference_endpoints
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'external';

ALTER TABLE inference_endpoints
    ADD CONSTRAINT chk_inference_endpoints_kind
    CHECK (kind IN ('dynamo', 'hosted', 'external'));

UPDATE inference_endpoints SET kind = 'dynamo' WHERE accepts_scheduling_priority;

COMMENT ON COLUMN inference_endpoints.kind IS
  'What kind of server this is: dynamo (self-hosted behind the dynamo frontend; receives the serving-class envelope), hosted (self-hosted, not dynamo), external (third-party provider; excluded for self_hosted_only organisations).';

-- ---------------------------------------------------------------------------
-- Analytics: what the request asked for and what it was served as. `resolved`
-- is set on every row that reached onwards' resolver; `requested` is NULL when
-- nothing named a class. Aggregates key on the resolved class.
ALTER TABLE http_analytics
    ADD COLUMN requested_serving_class TEXT,
    ADD COLUMN resolved_serving_class TEXT;

COMMENT ON COLUMN http_analytics.requested_serving_class IS
  'Serving class the request asked for (suffix > overlay default > account default); NULL when nothing named one.';
COMMENT ON COLUMN http_analytics.resolved_serving_class IS
  'Serving class the request was dispatched under (interactive | throughput | standard).';
