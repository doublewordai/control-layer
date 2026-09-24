-- Customer catalogue prices intentionally omit class-specific deals. Billing
-- continues to use effective_model_tariff with the resolved class.

-- Historical deals must survive account removal. Normal account deletion is
-- soft; a hard delete requires an explicit ledger-retention procedure.
ALTER TABLE model_tariffs DROP CONSTRAINT model_tariffs_user_id_fkey;
ALTER TABLE model_tariffs ADD CONSTRAINT model_tariffs_user_id_fkey
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE RESTRICT;
ALTER TABLE model_cache_tariffs DROP CONSTRAINT model_cache_tariffs_user_id_fkey;
ALTER TABLE model_cache_tariffs ADD CONSTRAINT model_cache_tariffs_user_id_fkey
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE RESTRICT;

CREATE FUNCTION effective_model_display_tariff(
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
           OR (purpose = 'playground' AND mt.api_key_purpose = 'realtime'))
      AND mt.completion_window IS NOT DISTINCT FROM
          CASE WHEN purpose = 'batch' THEN effective_model_display_tariff.completion_window ELSE NULL END
    ORDER BY CASE WHEN mt.user_id = account_id THEN 0 ELSE 1 END,
             CASE WHEN mt.api_key_purpose = COALESCE(purpose, 'realtime') THEN 0 ELSE 1 END,
             mt.valid_from DESC, mt.id
    LIMIT 1
$$;

-- Retained for compatibility with earlier pricing schemas. Admission does not
-- call this helper: zero customer deals do not exempt generally paid models.
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
                      AND purpose = 'batch' AND api_key_purpose = 'batch'
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
              SELECT 1 FROM (
                  SELECT NULL::TEXT AS completion_window WHERE purpose <> 'batch' OR purpose IS NULL
                  UNION SELECT DISTINCT completion_window FROM model_tariffs
                  WHERE purpose = 'batch' AND deployed_model_id = model_id
                    AND (user_id IS NULL OR user_id = account_id)
                    AND api_key_purpose = 'batch' AND completion_window IS NOT NULL
                    AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW())
              ) windows
              CROSS JOIN LATERAL effective_model_tariff(model_id, account_id, purpose,
                  windows.completion_window, legacy.serving_class, NOW()) tariff
          )
          AND (legacy.user_id IS NOT NULL OR NOT EXISTS (
              SELECT 1 FROM model_tariffs own
              WHERE own.deployed_model_id = model_id AND own.user_id = account_id
                AND own.api_key_purpose IS NULL AND own.serving_class IS NOT DISTINCT FROM legacy.serving_class
                AND own.valid_from <= NOW() AND (own.valid_until IS NULL OR own.valid_until > NOW())
          ))
    ))
$$;
