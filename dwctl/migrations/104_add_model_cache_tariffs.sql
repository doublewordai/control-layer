-- Cached-input pricing: per-model cache pricing as data, not code.
--
-- ONE row per model per pricing version, holding ALL three TTL tiers. This is a
-- deliberate design choice:
--   * Completeness by construction — a row carries 5m/1h/24h write multipliers as
--     NOT NULL columns, so a request can never hit a "missing tier" that would
--     default to a wrong (gameable) multiplier. There is no partial config to leak.
--   * Presence == enablement — a model has caching ON iff it has a row valid at the
--     request's time. There is no separate boolean flag to drift out of sync.
--   * Ledger — append-only, like model_tariffs. To change pricing you expire the
--     current row (set valid_until) and insert a new one; to disable you expire it
--     with no successor. Billing always resolves the row valid AS OF inference time,
--     so classify-time and (later, batched) billing-time agree even across a price
--     change, and the full history is retained for audit.
--
-- The multipliers apply to whatever base input rate the model_tariffs lookup resolves
-- (which already keys on api_key_purpose + completion_window), so batch stacking falls
-- out for free: a cache-read batch request bills at read_multiplier x batch_input_rate.
CREATE TABLE model_cache_tariffs (
    id                    UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    deployed_model_id     UUID         NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    -- All tiers present in one row → no missing-tier gap. x base input price.
    write_multiplier_5m   DECIMAL(6,4) NOT NULL,
    write_multiplier_1h   DECIMAL(6,4) NOT NULL,
    write_multiplier_24h  DECIMAL(6,4) NOT NULL,
    read_multiplier       DECIMAL(6,4) NOT NULL DEFAULT 0.1,  -- flat across tiers, data-driven
    min_prefix_tokens     INTEGER      NOT NULL,              -- per-model floor below which caching is skipped
    -- Ledger window. Never UPDATE/DELETE a row's pricing in place: insert a new
    -- version and expire the old one, so the as-of-inference lookup stays stable.
    valid_from            TIMESTAMPTZ  NOT NULL DEFAULT now(),
    valid_until           TIMESTAMPTZ,
    UNIQUE (deployed_model_id, valid_from)
);

-- At most one ACTIVE (un-expired) version per model. This is the integrity backstop the
-- ledger relies on: enable() expires the current version then inserts a new one, but two
-- concurrent enables can each see "no active row" and both insert (their valid_from differs,
-- so the UNIQUE above doesn't catch it) → two active rows → an ambiguous as-of lookup. The
-- partial unique index makes the second INSERT fail, serialising enables. Mirrors
-- model_tariffs (042_add_model_tariffs.sql).
CREATE UNIQUE INDEX idx_model_cache_tariffs_unique_active
    ON model_cache_tariffs (deployed_model_id)
    WHERE valid_until IS NULL;

CREATE INDEX idx_model_cache_tariffs_model ON model_cache_tariffs (deployed_model_id);
