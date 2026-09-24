-- Internal resume and management keys never select customer prices. Preserve
-- historical rows; only replace the pricing functions from migration 151.

CREATE OR REPLACE FUNCTION effective_model_tariff(
    model_id UUID, account_id UUID, purpose TEXT, completion_window TEXT,
    resolved_class TEXT, at_time TIMESTAMPTZ
) RETURNS SETOF model_tariffs LANGUAGE SQL STABLE AS $$
    SELECT mt.id, mt.deployed_model_id, mt.name, mt.input_price_per_token,
           mt.output_price_per_token, mt.valid_from, mt.valid_until, mt.api_key_purpose,
           mt.completion_window, mt.user_id, mt.serving_class
    FROM model_tariffs mt
    WHERE COALESCE(purpose, 'realtime') IN ('realtime','batch','playground')
      AND mt.deployed_model_id = model_id
      AND (mt.user_id = account_id OR mt.user_id IS NULL)
      AND (mt.serving_class = resolved_class OR mt.serving_class IS NULL)
      AND mt.valid_from <= at_time AND (mt.valid_until IS NULL OR mt.valid_until > at_time)
      AND ((mt.api_key_purpose = COALESCE(purpose, 'realtime')
            AND (mt.completion_window IS NULL OR mt.completion_window = effective_model_tariff.completion_window))
           OR (mt.api_key_purpose = 'realtime' AND mt.completion_window IS NULL))
    ORDER BY CASE WHEN mt.user_id = account_id AND mt.serving_class = resolved_class THEN 0
                  WHEN mt.user_id = account_id THEN 1 ELSE 2 END,
             CASE WHEN mt.api_key_purpose = COALESCE(purpose, 'realtime')
                        AND mt.completion_window = effective_model_tariff.completion_window THEN 0
                  WHEN mt.api_key_purpose = COALESCE(purpose, 'realtime') THEN 1 ELSE 2 END,
             mt.valid_from DESC, mt.id
    LIMIT 1
$$;

-- Pool admission remains model-level: a key needs credit whenever any applicable
-- class/window is chargeable. Do not treat a paid class as free just because the
-- general model price is zero, or a zero-price deal as missing.
CREATE OR REPLACE FUNCTION model_has_effective_paid_tariff(model_id UUID, account_id UUID, purpose TEXT)
RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
    SELECT COALESCE(purpose, 'realtime') IN ('realtime','batch','playground') AND (EXISTS (
        SELECT 1 FROM (
            SELECT CASE WHEN purpose = 'batch' THEN 'standard' ELSE NULL::TEXT END AS name
            UNION SELECT DISTINCT serving_class FROM model_tariffs
                  WHERE deployed_model_id = model_id AND user_id = account_id
                    AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW())
                    AND purpose <> 'batch'
        ) classes
        CROSS JOIN (SELECT NULL::TEXT AS completion_window UNION
                    SELECT DISTINCT completion_window FROM model_tariffs
                    WHERE deployed_model_id = model_id AND (user_id IS NULL OR user_id = account_id)
                      AND api_key_purpose = purpose
                      AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW())) windows
        CROSS JOIN LATERAL effective_model_tariff(model_id, account_id, purpose, windows.completion_window, classes.name, NOW()) tariff
        WHERE tariff.input_price_per_token > 0 OR tariff.output_price_per_token > 0
    )
    -- Legacy purpose-less rows historically made a model metered. Preserve that
    -- admission guard when there is no effective purpose-specific price; do not
    -- invent a new billable tariff or defeat an explicit zero-price deal.
    OR EXISTS (
        SELECT 1 FROM model_tariffs legacy
        WHERE legacy.deployed_model_id = model_id AND legacy.api_key_purpose IS NULL
          AND (legacy.user_id IS NULL OR legacy.user_id = account_id)
          AND legacy.valid_from <= NOW() AND (legacy.valid_until IS NULL OR legacy.valid_until > NOW())
          AND (legacy.input_price_per_token > 0 OR legacy.output_price_per_token > 0)
          AND NOT EXISTS (
              SELECT 1 FROM effective_model_tariff(model_id, account_id, purpose, NULL, legacy.serving_class, NOW())
          )
          AND (legacy.user_id IS NOT NULL OR NOT EXISTS (
              SELECT 1 FROM model_tariffs own
              WHERE own.deployed_model_id = model_id AND own.user_id = account_id
                AND own.api_key_purpose IS NULL AND own.serving_class IS NOT DISTINCT FROM legacy.serving_class
                AND own.valid_from <= NOW() AND (own.valid_until IS NULL OR own.valid_until > NOW())
          ))
    ))
$$;
