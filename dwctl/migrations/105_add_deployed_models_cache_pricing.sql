-- Cached-input pricing: per-model enablement gate (plan §6.6 / §8.5).
--
-- Caching is disabled by default and only flipped true once the model's tokenizer
-- is live in tokenizer-svc (the sole source of token counts, §6.5). The classifier
-- skips any model with this false: cache_control markers are accepted but treated
-- as no-ops (no cache, full price, no error) — so customer code never breaks.
ALTER TABLE deployed_models
    ADD COLUMN cache_pricing_enabled BOOLEAN NOT NULL DEFAULT false;
