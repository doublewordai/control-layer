-- Migration 117 (COR-524): denormalize per-batch analytics (tokens / latency / cost)
-- onto batch_aggregates so GET /batches/{id}/analytics can be served from this small
-- per-batch read model instead of scanning raw http_analytics.
--
-- Today get_batch_analytics (and its bulk sibling) COUNT + SUM tokens, AVG duration/ttfb
-- and SUM cost by filtering http_analytics on fusillade_batch_id — the sole remaining
-- reader of idx_analytics_fusillade_batch_id (1.6 GB). batch_aggregates already carries
-- the per-batch total_amount / transaction_count / service_tier (one row per batch, small),
-- so carrying the token/latency/cost aggregates here too lets us repoint those reads and
-- drop that index (the contract PR).
--
-- All aggregates are additive (foldable): the analytics batcher folds each flush's batched
-- requests into these columns in the same transaction it writes total_amount, and a one-off
-- backfill (scripts/backfill_batch_analytics_denorm.sh) fills historical batches from raw.
-- Latency is stored as sum + count (not an average) precisely because AVG is not foldable;
-- the endpoint divides at read time.
--
-- Aggregated set = the batch's *successful (status 2xx)* requests — including free /
-- zero-priced ones, matching what get_batch_analytics historically COUNT/SUM'd off raw
-- http_analytics. This is deliberately BROADER than the billed set the credit fold uses
-- (priced, cost > 0): a free-model batch has no credit rows but must still report its
-- tokens/latency. Idempotency therefore cannot ride the credits dedup; the go-forward fold
-- rides the http_analytics upsert's newly-inserted rows (xmax = 0) instead, and the backfill
-- SETs absolute aggregates over quiescent batches — so historical and go-forward rows share
-- the same "all 2xx" definition. `total_requests` is that 2xx count (distinct from
-- transaction_count, which stays the billed-row count).

ALTER TABLE batch_aggregates
    -- Count of successful (2xx) requests folded — the analytics request count, distinct from
    -- transaction_count (billed rows). Equal for priced batches; larger for batches with free
    -- requests. Read as `total_requests` by get_batch_analytics.
    ADD COLUMN total_requests           BIGINT NOT NULL DEFAULT 0,
    -- Token totals (SUM over the batch's successful requests).
    ADD COLUMN total_prompt_tokens     BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN total_completion_tokens  BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN total_reasoning_tokens   BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN total_tokens             BIGINT NOT NULL DEFAULT 0,
    -- Latency stored as sum + count so AVG is foldable; count tracks only requests that
    -- reported the metric (duration is always present; ttfb can be NULL), matching the
    -- current SQL AVG which ignores NULLs.
    ADD COLUMN sum_duration_ms          BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN count_duration_ms        BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN sum_ttfb_ms              BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN count_ttfb_ms            BIGINT NOT NULL DEFAULT 0,
    -- List price = SUM(prompt·input_price + completion·output_price), i.e. the un-discounted
    -- cost the endpoint reports today (http_analytics.uncached_cost). Distinct from
    -- total_amount, which is the cache-adjusted *billed* cost.
    ADD COLUMN total_list_cost          NUMERIC NOT NULL DEFAULT 0,
    -- One-off backfill bookkeeping: stamped when a historical batch's aggregates are filled
    -- from raw. Nullable so the backfill can guard on `IS NULL` for idempotency/resumability
    -- (the aggregate columns themselves are NOT NULL for the fold's `+=`, so they can't serve
    -- as the guard). NULL on a live fold-maintained row is expected and harmless.
    ADD COLUMN analytics_backfilled_at  TIMESTAMPTZ;
