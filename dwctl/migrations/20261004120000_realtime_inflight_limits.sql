SET LOCAL lock_timeout = '5s';

ALTER TABLE deployed_models
    ADD COLUMN realtime_inflight_limit INTEGER NOT NULL DEFAULT 14
    CONSTRAINT chk_deployed_models_realtime_inflight_limit CHECK (realtime_inflight_limit > 0);

CREATE TABLE realtime_inflight_limit_overrides (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    inflight_limit INTEGER,
    reason TEXT NOT NULL,
    set_by UUID NOT NULL REFERENCES users(id),
    valid_from TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_until TIMESTAMPTZ,
    CONSTRAINT chk_realtime_inflight_limit_overrides_limit CHECK (inflight_limit IS NULL OR inflight_limit > 0),
    CONSTRAINT chk_realtime_inflight_limit_overrides_period CHECK (valid_until IS NULL OR valid_until >= valid_from),
    CONSTRAINT chk_realtime_inflight_limit_overrides_reason CHECK (length(btrim(reason)) > 0)
);

CREATE UNIQUE INDEX idx_realtime_inflight_limit_overrides_current
    ON realtime_inflight_limit_overrides (deployed_model_id, user_id)
    WHERE valid_until IS NULL;

CREATE INDEX idx_realtime_inflight_limit_overrides_user_id
    ON realtime_inflight_limit_overrides (user_id);

CREATE TRIGGER realtime_inflight_limit_overrides_notify
    AFTER INSERT OR UPDATE OR DELETE ON realtime_inflight_limit_overrides
    EXECUTE FUNCTION notify_config_change();
