-- Organisation prices may specialize a resolved serving class. NULL remains an
-- all-class deal, distinct from an explicit standard-class price.
-- The pre-rollout migration Job bounds execution time.
-- Bound lock waits so queued DDL does not block live billing and admission reads.
SET LOCAL lock_timeout = '5s';

ALTER TABLE model_tariffs ADD COLUMN serving_class TEXT;
ALTER TABLE model_cache_tariffs ADD COLUMN serving_class TEXT;
ALTER TABLE model_tariffs ADD CONSTRAINT model_tariffs_class_scope
    CHECK (serving_class IS NULL OR (user_id IS NOT NULL AND serving_class IN ('standard','interactive','throughput','custom')));
ALTER TABLE model_cache_tariffs ADD CONSTRAINT model_cache_tariffs_class_scope
    CHECK (serving_class IS NULL OR (user_id IS NOT NULL AND serving_class IN ('standard','interactive','throughput','custom')));

-- Final class-aware indexes are built concurrently after the schema additions.

-- Shared by admission, price display, sorting and estimates. Billing's in-memory
-- resolver uses this same order: the whole purpose/window fallback within each
-- scope, then the next scope. A zero price is an explicit match.
CREATE FUNCTION effective_model_tariff(
    model_id UUID, account_id UUID, purpose TEXT, completion_window TEXT,
    resolved_class TEXT, at_time TIMESTAMPTZ
) RETURNS SETOF model_tariffs LANGUAGE SQL STABLE AS $$
    SELECT mt.* FROM model_tariffs mt
    WHERE mt.deployed_model_id = model_id
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
CREATE FUNCTION model_has_effective_paid_tariff(model_id UUID, account_id UUID, purpose TEXT)
RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
    SELECT EXISTS (
        SELECT 1 FROM (
            SELECT CASE WHEN purpose IN ('batch','continuation') THEN 'standard' ELSE NULL::TEXT END AS name
            UNION SELECT DISTINCT serving_class FROM model_tariffs
                  WHERE deployed_model_id = model_id AND user_id = account_id
                    AND valid_from <= NOW() AND (valid_until IS NULL OR valid_until > NOW())
                    AND purpose NOT IN ('batch','continuation')
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
    )
$$;
