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

-- An account's class-less deal covers the standard class only: any other
-- class is priced by the account's deal for that class, else the model's.
CREATE OR REPLACE FUNCTION effective_model_tariff(
    model_id UUID, account_id UUID, purpose TEXT, completion_window TEXT,
    resolved_class TEXT, at_time TIMESTAMPTZ
) RETURNS SETOF model_tariffs LANGUAGE SQL STABLE AS $$
    SELECT mt.id, mt.deployed_model_id, mt.name, mt.input_price_per_token, mt.output_price_per_token, mt.valid_from, mt.valid_until, mt.api_key_purpose, mt.completion_window, mt.user_id, mt.serving_class, mt.provisioning_source
    FROM model_tariffs mt
    WHERE COALESCE(purpose, 'realtime') IN ('realtime','batch','playground')
      AND mt.deployed_model_id = model_id
      AND (mt.user_id = account_id OR mt.user_id IS NULL)
      AND (mt.user_id IS NOT NULL OR mt.serving_class IS NULL OR EXISTS (
          SELECT 1 FROM deployed_models dm WHERE dm.id=model_id AND dm.routing_mode='class_routes'))
      AND (mt.serving_class = CASE WHEN purpose = 'batch' THEN 'standard' ELSE resolved_class END OR mt.serving_class IS NULL)
      AND NOT (mt.user_id IS NOT NULL AND mt.serving_class IS NULL
               AND COALESCE(CASE WHEN purpose = 'batch' THEN 'standard' ELSE resolved_class END, 'standard') <> 'standard')
      AND mt.valid_from <= at_time AND (mt.valid_until IS NULL OR mt.valid_until > at_time)
      AND (mt.api_key_purpose = COALESCE(purpose, 'realtime')
           OR (purpose IN ('playground','batch') AND mt.api_key_purpose = 'realtime'))
      AND mt.completion_window IS NOT DISTINCT FROM
          CASE WHEN mt.api_key_purpose = 'batch' THEN effective_model_tariff.completion_window ELSE NULL END
    ORDER BY CASE WHEN purpose = 'batch' AND mt.api_key_purpose = 'realtime' THEN 1 ELSE 0 END,
             CASE WHEN mt.user_id = account_id AND mt.serving_class = CASE WHEN purpose = 'batch' THEN 'standard' ELSE resolved_class END THEN 0
                  WHEN mt.user_id = account_id THEN 1
                  WHEN mt.serving_class IS NOT NULL THEN 2 ELSE 3 END,
             CASE WHEN mt.api_key_purpose = COALESCE(purpose, 'realtime') THEN 0 ELSE 1 END,
             mt.valid_from DESC, mt.id
    LIMIT 1
$$;
