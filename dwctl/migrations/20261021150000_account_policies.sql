-- Per-account serving policy from the account catalog files: an account-wide
-- scheduling priority, and per-model or per-class priority and latency targets.
-- Written only by the account catalog reconciler.
SET LOCAL lock_timeout = '5s';

ALTER TABLE users ADD COLUMN scheduling_priority INTEGER;

COMMENT ON COLUMN users.scheduling_priority IS
  'Account setting from the account catalog: the scheduling priority sent with this account''s realtime requests to self-hosted serving members, unless a model or class policy overrides it. NULL sends the default.';

CREATE TRIGGER users_scheduling_priority_notify
    AFTER UPDATE OF scheduling_priority ON users
    FOR EACH ROW
    WHEN (OLD.scheduling_priority IS DISTINCT FROM NEW.scheduling_priority)
    EXECUTE FUNCTION notify_config_change();

CREATE TABLE account_model_policies (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    -- NULL applies to every class of the model; a class key narrows it.
    serving_class TEXT,
    priority INTEGER,
    ttft_ms INTEGER CONSTRAINT chk_account_model_policies_ttft CHECK (ttft_ms > 0),
    itl_ms INTEGER CONSTRAINT chk_account_model_policies_itl CHECK (itl_ms > 0),
    provisioning_source TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_account_model_policies_scope UNIQUE NULLS NOT DISTINCT (user_id, deployed_model_id, serving_class)
);

COMMENT ON TABLE account_model_policies IS
  'Per-account serving policy on one model (serving_class NULL) or one of its classes, from the account catalog files.';

CREATE INDEX idx_account_model_policies_model ON account_model_policies (deployed_model_id);

CREATE TRIGGER account_model_policies_notify
    AFTER INSERT OR UPDATE OR DELETE ON account_model_policies
    EXECUTE FUNCTION notify_config_change();
