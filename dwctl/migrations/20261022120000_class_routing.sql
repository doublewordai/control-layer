-- Class routing: a class's own realtime limit, per-account limits on classes
-- other than standard, and config notifications when classes change. All
-- additive; nothing routes on these until a model is set to class_routes.
SET LOCAL lock_timeout = '5s';

ALTER TABLE model_serving_classes ADD COLUMN realtime_inflight_limit INTEGER
    CONSTRAINT chk_model_serving_classes_realtime_inflight_limit CHECK (realtime_inflight_limit > 0);

COMMENT ON COLUMN model_serving_classes.realtime_inflight_limit IS
  'Concurrent realtime requests one account may have in flight on this class. NULL uses the model''s realtime_inflight_limit.';

COMMENT ON COLUMN deployed_models.routing_mode IS
  'legacy routes through the model''s composite; class_routes routes each serving class to its own upstream. Set by the model catalog.';

CREATE TRIGGER model_serving_classes_notify
    AFTER INSERT OR UPDATE OR DELETE ON model_serving_classes
    EXECUTE FUNCTION notify_config_change();

-- Per-account realtime limits on a class other than standard. The standard
-- class keeps realtime_inflight_limit_overrides, so its limiter key and rows
-- are unchanged when a model is activated.
CREATE TABLE realtime_inflight_class_overrides (
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    class_key TEXT NOT NULL CONSTRAINT chk_realtime_inflight_class_overrides_class CHECK (class_key ~ '^[a-z][a-z0-9_-]*$' AND class_key <> 'standard'),
    inflight_limit INTEGER NOT NULL CONSTRAINT chk_realtime_inflight_class_overrides_limit CHECK (inflight_limit > 0),
    PRIMARY KEY (deployed_model_id, user_id, class_key)
);

COMMENT ON TABLE realtime_inflight_class_overrides IS
  'Per-account realtime in-flight limits on a non-standard serving class, from the account catalog. Standard-class limits live in realtime_inflight_limit_overrides.';

CREATE INDEX idx_realtime_inflight_class_overrides_user_id ON realtime_inflight_class_overrides (user_id);

CREATE TRIGGER realtime_inflight_class_overrides_notify
    AFTER INSERT OR UPDATE OR DELETE ON realtime_inflight_class_overrides
    EXECUTE FUNCTION notify_config_change();
