-- Migration: Per-day per-user per-model usage rollup (`user_model_usage_daily`).
--
-- This table supersedes `user_model_usage` (migration 069) as the SINGLE usage
-- rollup. All-time usage is a SUM over every day for a (user, model); a date
-- range is the same query with a `usage_date` filter. Serving long-range reads
-- from this compact rollup means raw `http_analytics` is never scanned for usage,
-- which is what lets a later phase (COR-509) partition `http_analytics` and cut
-- its retention window without breaking historical `/usage`.
--
-- Summable metrics only. Tokens/cost/request_count sum across days, so splitting a
-- day's rows across incremental refresh windows still totals correctly. We do NOT
-- store batch_count here: COUNT(DISTINCT fusillade_batch_id) is not additive across
-- day boundaries -- batch counts over any range come from `batch_aggregates`
-- (per-batch, has created_at). Latency percentiles (p95/p99) are likewise not
-- summable; they stay capped to the retention window and are served from raw.
--
-- Rollout is expand/contract: this migration only ADDS the table + cursor (it does
-- NOT drop `user_model_usage`). The daily table is populated forward by the refresh
-- daemon and backfilled for history by a one-off script; reads are cut over and the
-- old table dropped (migration 111) only after the backfill is validated.

CREATE TABLE user_model_usage_daily (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    usage_date DATE NOT NULL,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cost DECIMAL(24, 15) NOT NULL DEFAULT 0,
    request_count BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- PK leads with user_id so it serves both the all-time read (WHERE user_id)
    -- and the date-range read (WHERE user_id AND usage_date BETWEEN ...).
    PRIMARY KEY (user_id, model, usage_date)
);

-- Single-row cursor for the incremental refresh.
--
--   last_processed_id  -- highest http_analytics.id folded into the daily table so
--                         far; the refresh daemon only scans WHERE id > this. Seeded
--                         to the current MAX(id) so the daemon starts forward-only
--                         and never scans history on first run.
--   backfill_watermark -- IMMUTABLE boundary captured atomically here. The one-off
--                         backfill fills WHERE id <= backfill_watermark; the daemon
--                         only ever touches WHERE id > last_processed_id (>= this
--                         watermark). The two id-ranges are disjoint, so the backfill
--                         and the live daemon can run concurrently with no double
--                         count -- a day straddling the boundary gets each side's
--                         contribution summed via the additive ON CONFLICT upsert.
CREATE TABLE user_model_usage_daily_cursor (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id = TRUE),
    last_processed_id BIGINT NOT NULL,
    backfill_watermark BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

INSERT INTO user_model_usage_daily_cursor (last_processed_id, backfill_watermark)
SELECT COALESCE(MAX(id), 0), COALESCE(MAX(id), 0) FROM http_analytics;
