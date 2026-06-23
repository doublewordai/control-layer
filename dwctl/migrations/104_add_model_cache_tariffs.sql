-- Cached-input pricing: per-model cache pricing as data, not code (plan §8.3).
--
-- The multipliers apply to whatever base input rate the existing model_tariffs
-- lookup resolves (which already keys on api_key_purpose + completion_window), so
-- batch stacking falls out for free: a cache-hit batch request bills at
-- read_multiplier x batch_input_rate. No special-casing.
CREATE TABLE model_cache_tariffs (
    id                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    deployed_model_id UUID        NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    ttl_tier          TEXT        NOT NULL CHECK (ttl_tier IN ('5m', '1h', '24h')),
    write_multiplier  DECIMAL(6,4) NOT NULL,              -- e.g. 1.25 / 2.0 / 2.5 x base input price
    read_multiplier   DECIMAL(6,4) NOT NULL DEFAULT 0.1,  -- flat 10% today, but data-driven
    min_prefix_tokens INTEGER     NOT NULL,               -- per-model floor below which caching is skipped (§1)
    valid_from        TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_until       TIMESTAMPTZ,
    UNIQUE (deployed_model_id, ttl_tier, valid_from)
);

CREATE INDEX idx_model_cache_tariffs_model ON model_cache_tariffs (deployed_model_id);
