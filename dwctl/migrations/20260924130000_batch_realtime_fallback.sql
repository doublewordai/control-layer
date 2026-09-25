-- Exact batch-window prices win across every scope before realtime fallback.
-- The fallback uses standard-class, all-class account, then general realtime
-- prices. Playground retains its scope-first realtime fallback. Zero is a match.
-- Billing, estimates and display queries must agree on the final safety net.
SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION effective_model_tariff(
    model_id UUID, account_id UUID, purpose TEXT, completion_window TEXT,
    resolved_class TEXT, at_time TIMESTAMPTZ
) RETURNS SETOF model_tariffs LANGUAGE SQL STABLE AS $$
    SELECT mt.*
    FROM model_tariffs mt
    WHERE COALESCE(purpose, 'realtime') IN ('realtime','batch','playground')
      AND mt.deployed_model_id = model_id
      AND (mt.user_id = account_id OR mt.user_id IS NULL)
      AND (mt.serving_class = CASE WHEN purpose = 'batch' THEN 'standard' ELSE resolved_class END OR mt.serving_class IS NULL)
      AND mt.valid_from <= at_time AND (mt.valid_until IS NULL OR mt.valid_until > at_time)
      AND (mt.api_key_purpose = COALESCE(purpose, 'realtime')
           OR (purpose IN ('playground','batch') AND mt.api_key_purpose = 'realtime'))
      AND mt.completion_window IS NOT DISTINCT FROM
          CASE WHEN mt.api_key_purpose = 'batch' THEN effective_model_tariff.completion_window ELSE NULL END
    ORDER BY CASE WHEN purpose = 'batch' AND mt.api_key_purpose = 'realtime' THEN 1 ELSE 0 END,
             CASE WHEN mt.user_id = account_id AND mt.serving_class = CASE WHEN purpose = 'batch' THEN 'standard' ELSE resolved_class END THEN 0
                  WHEN mt.user_id = account_id THEN 1 ELSE 2 END,
             CASE WHEN mt.api_key_purpose = COALESCE(purpose, 'realtime') THEN 0 ELSE 1 END,
             mt.valid_from DESC, mt.id
    LIMIT 1
$$;

CREATE OR REPLACE FUNCTION effective_model_display_tariff(
    model_id UUID, account_id UUID, purpose TEXT, completion_window TEXT,
    at_time TIMESTAMPTZ
) RETURNS SETOF model_tariffs LANGUAGE SQL STABLE AS $$
    SELECT mt.* FROM model_tariffs mt
    WHERE COALESCE(purpose, 'realtime') IN ('realtime','batch','playground')
      AND mt.deployed_model_id = model_id
      AND (mt.user_id = account_id OR mt.user_id IS NULL)
      AND mt.serving_class IS NULL
      AND mt.valid_from <= at_time AND (mt.valid_until IS NULL OR mt.valid_until > at_time)
      AND (mt.api_key_purpose = COALESCE(purpose, 'realtime')
           OR (purpose IN ('playground','batch') AND mt.api_key_purpose = 'realtime'))
      AND mt.completion_window IS NOT DISTINCT FROM
          CASE WHEN mt.api_key_purpose = 'batch' THEN effective_model_display_tariff.completion_window ELSE NULL END
    ORDER BY CASE WHEN purpose = 'batch' AND mt.api_key_purpose = 'realtime' THEN 1 ELSE 0 END,
             CASE WHEN mt.user_id = account_id THEN 0 ELSE 1 END,
             CASE WHEN mt.api_key_purpose = COALESCE(purpose, 'realtime') THEN 0 ELSE 1 END,
             mt.valid_from DESC, mt.id
    LIMIT 1
$$;

