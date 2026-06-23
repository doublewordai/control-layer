-- Cached-input pricing on the analytics/billing path (plan §8.2/§8.4).
--
-- Records the per-request cache token split (read + per-tier creation) and makes the
-- request cost cache-aware. `prompt_tokens` is unchanged: it stays the full input count
-- (= cache_read + cache_creation + uncached), preserving OpenAI semantics. The discount
-- is reflected in the cost, not the token counts.
--
-- ONLINE-SAFE / METADATA-ONLY by design: every statement here is a catalog change that
-- does NOT rewrite the (potentially huge) http_analytics table — only a brief
-- ACCESS EXCLUSIVE lock to update the catalog. Verified by relfilenode-invariance:
--   * ADD COLUMN ... NOT NULL DEFAULT <constant>  — PG 11+ stores the default in the
--     catalog, applied virtually; no rewrite.
--   * ADD COLUMN ... (nullable, no default)       — catalog-only; no rewrite.
--   * ALTER COLUMN ... DROP EXPRESSION            — drops the generation rule, keeps the
--     already-materialised data; no rewrite.
-- Deliberately AVOIDED: adding a STORED generated column, which DOES rewrite the whole
-- table under ACCESS EXCLUSIVE (cannot be batched). `uncached_cost` is therefore a plain
-- column the batcher writes going forward; historical rows are filled by the resumable
-- batched side-script `scripts/backfill_uncached_cost.sh` (run once per DB).

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
-- list price; new rows get the cache-adjusted cost). No table rewrite.
ALTER TABLE http_analytics ALTER COLUMN total_cost DROP EXPRESSION;

-- New: the un-discounted list price, as a plain batcher-written column (NOT generated —
-- a generated-STORED add would rewrite the whole table). NULL for historical rows until
-- the backfill runs; for new rows the batcher writes it. savings = uncached_cost - total_cost.
ALTER TABLE http_analytics ADD COLUMN uncached_cost DECIMAL(12, 8);

-- Fidelity bridge for the rollout window. Once total_cost stops being a generated column,
-- the OLD release (still serving during a blue/green cutover, and longer if the rollout
-- stalls or rolls back) inserts rows WITHOUT writing total_cost — they would land NULL.
-- This trigger reconstructs the dropped generation expression as a fallback: if an inserter
-- leaves total_cost NULL, fill it from the row's own tokens x prices. The old release (no
-- caching) thus gets the correct list price = charged; the new release writes the
-- cache-adjusted value and the trigger no-ops. NULL prices (free / un-tariffed models)
-- propagate to NULL, exactly as the old CASE expression did. total_cost is the ANALYTICS
-- column (billed `amount` is computed independently in code), so this is fidelity, not
-- billing safety — but it keeps analytics whole across the cutover.
-- FOLLOW-UP: drop this trigger + function in a later migration once the old release is gone.
CREATE OR REPLACE FUNCTION http_analytics_fill_total_cost() RETURNS trigger AS $$
BEGIN
    IF NEW.total_cost IS NULL THEN
        NEW.total_cost := NEW.prompt_tokens * NEW.input_price_per_token
                        + NEW.completion_tokens * NEW.output_price_per_token;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER http_analytics_fill_total_cost_trg
    BEFORE INSERT ON http_analytics
    FOR EACH ROW
    EXECUTE FUNCTION http_analytics_fill_total_cost();
