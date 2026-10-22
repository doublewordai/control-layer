SET LOCAL lock_timeout = '5s';

ALTER TABLE deployed_models
    ADD COLUMN realtime_inflight_limit INTEGER NOT NULL DEFAULT 14
    CONSTRAINT chk_deployed_models_realtime_inflight_limit CHECK (realtime_inflight_limit > 0);

CREATE TABLE realtime_inflight_limit_overrides (
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    inflight_limit INTEGER NOT NULL,
    PRIMARY KEY (deployed_model_id, user_id),
    CONSTRAINT chk_realtime_inflight_limit_overrides_limit CHECK (inflight_limit > 0)
);

CREATE INDEX idx_realtime_inflight_limit_overrides_user_id
    ON realtime_inflight_limit_overrides (user_id);

CREATE TRIGGER realtime_inflight_limit_overrides_notify
    AFTER INSERT OR UPDATE OR DELETE ON realtime_inflight_limit_overrides
    EXECUTE FUNCTION notify_config_change();
