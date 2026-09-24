-- Exhaust each account/class scope before falling back to general model prices.
-- Only playground may fall back to realtime, within that same scope. Batch stays
-- isolated by purpose and exact completion window. Quotes and sorting that
-- use this resolver follow the same account-first precedence, including zero.

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
           OR (purpose = 'playground' AND mt.api_key_purpose = 'realtime'))
      AND mt.completion_window IS NOT DISTINCT FROM
          CASE WHEN purpose = 'batch' THEN effective_model_tariff.completion_window ELSE NULL END
    ORDER BY CASE WHEN mt.user_id = account_id AND mt.serving_class = CASE WHEN purpose = 'batch' THEN 'standard' ELSE resolved_class END THEN 0
                  WHEN mt.user_id = account_id THEN 1 ELSE 2 END,
             CASE WHEN mt.api_key_purpose = COALESCE(purpose, 'realtime') THEN 0 ELSE 1 END,
             mt.valid_from DESC, mt.id
    LIMIT 1
$$;
