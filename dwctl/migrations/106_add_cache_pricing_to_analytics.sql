-- Cached-input pricing on the analytics/billing path (plan §8.2/§8.4).
--
-- Records the per-request cache token split (read + per-tier creation) and makes the
-- request cost cache-aware. `prompt_tokens` is unchanged: it stays the full input count
-- (= cache_read + cache_creation + uncached), preserving OpenAI semantics. The discount
-- is reflected in the cost, not the token counts.

ALTER TABLE http_analytics
  ADD COLUMN cache_read_input_tokens         BIGINT NOT NULL DEFAULT 0,
  ADD COLUMN cache_creation_input_tokens     BIGINT NOT NULL DEFAULT 0,
  ADD COLUMN cache_creation_5m_input_tokens  BIGINT NOT NULL DEFAULT 0,
  ADD COLUMN cache_creation_1h_input_tokens  BIGINT NOT NULL DEFAULT 0,
  ADD COLUMN cache_creation_24h_input_tokens BIGINT NOT NULL DEFAULT 0;

-- `total_cost` was a GENERATED column holding the list price. The cache-adjusted cost
-- depends on the per-tier multipliers in `model_cache_tariffs`, which a generated
-- expression cannot reference, so it becomes a batcher-written column carrying the real
-- (discounted) cost — equal to the billed `credits_transactions.amount`. DROP EXPRESSION
-- converts it in place and PRESERVES every existing value (historical rows keep their
-- list price; new rows get the cache-adjusted cost).
ALTER TABLE http_analytics ALTER COLUMN total_cost DROP EXPRESSION;

-- The un-discounted list price is preserved in a new GENERATED column, so dashboards can
-- compute savings = uncached_cost - total_cost (plan §13). Same CASE/NULL semantics as
-- the original total_cost expression.
ALTER TABLE http_analytics
  ADD COLUMN uncached_cost DECIMAL(12, 8) GENERATED ALWAYS AS (
      CASE
          WHEN prompt_tokens IS NOT NULL AND input_price_per_token IS NOT NULL
               AND completion_tokens IS NOT NULL AND output_price_per_token IS NOT NULL
          THEN (prompt_tokens * input_price_per_token) + (completion_tokens * output_price_per_token)
          ELSE NULL
      END
  ) STORED;
