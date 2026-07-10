-- Validate user_model_usage_daily against RAW http_analytics before the read cutover
-- (COR-506). Both are additive sums over the same 2xx http_analytics rows, so once the
-- daemon has folded up to its cursor, the per (user, model) totals must match.
--
-- Validate against RAW, never against the old `user_model_usage` table: that table is a
-- long-lived accumulator that has drifted ~13% high (up to ~25x on individual pairs) from
-- what http_analytics actually contains. The daily rollup, rebuilt from raw, is the more
-- correct number; raw is the source of truth.
--
-- Scope: only the retention window (:window_days, default 35). Once http_analytics
-- retention is cut (COR-509), raw only covers recent days, so daily-vs-raw is only
-- checkable there; older history is validated at backfill time and then trusted as
-- immutable. The raw side is also bounded to id <= the daemon cursor, so rows the daemon
-- has not folded yet don't show up as false diffs.
--
-- Override the window with:  psql ... -v window_days=60 -f this.sql
--
-- Expected output: zero rows, or a handful of tiny diffs on currently-active users (the
-- in-flight tail between the cursor snapshot and the scan). Large diffs, or whole
-- (user, model) pairs missing on one side, indicate a real problem — do NOT cut reads over
-- until they're understood.

\set window_days 35

WITH bounds AS (
    -- Both sides compare on UTC dates (usage_date is UTC; the raw side extracts
    -- (timestamp AT TIME ZONE 'UTC')::date), so anchor the window to the UTC
    -- current date too — CURRENT_DATE follows the session timezone and would
    -- shift the window by a day on a non-UTC psql session.
    SELECT ((now() AT TIME ZONE 'UTC')::date - (:window_days)::int)                    AS min_date,
           (SELECT last_processed_id FROM user_model_usage_daily_cursor WHERE id = TRUE) AS max_id
),
daily AS (
    SELECT user_id, model,
           SUM(input_tokens)  AS input_tokens,
           SUM(output_tokens) AS output_tokens,
           SUM(cost)          AS cost,
           SUM(request_count) AS request_count
    FROM user_model_usage_daily, bounds
    WHERE usage_date >= bounds.min_date
    GROUP BY user_id, model
),
raw AS (
    SELECT user_id, model,
           COALESCE(SUM(prompt_tokens), 0)     AS input_tokens,
           COALESCE(SUM(completion_tokens), 0) AS output_tokens,
           COALESCE(SUM(total_cost), 0)        AS cost,
           COUNT(*)                            AS request_count
    FROM http_analytics, bounds
    WHERE id <= bounds.max_id
      AND (timestamp AT TIME ZONE 'UTC')::date >= bounds.min_date
      AND user_id IS NOT NULL AND model IS NOT NULL
      AND status_code BETWEEN 200 AND 299
    GROUP BY user_id, model
)
SELECT
    COALESCE(d.user_id, r.user_id)          AS user_id,
    COALESCE(d.model, r.model)              AS model,
    r.request_count                         AS raw_requests,
    d.request_count                         AS daily_requests,
    COALESCE(d.request_count, 0) - COALESCE(r.request_count, 0) AS request_diff,
    COALESCE(d.input_tokens, 0)  - COALESCE(r.input_tokens, 0)  AS input_token_diff,
    COALESCE(d.output_tokens, 0) - COALESCE(r.output_tokens, 0) AS output_token_diff,
    COALESCE(d.cost, 0)          - COALESCE(r.cost, 0)          AS cost_diff
FROM daily d
FULL OUTER JOIN raw r USING (user_id, model)
WHERE COALESCE(d.request_count, 0) <> COALESCE(r.request_count, 0)
   OR COALESCE(d.input_tokens, 0)  <> COALESCE(r.input_tokens, 0)
   OR COALESCE(d.output_tokens, 0) <> COALESCE(r.output_tokens, 0)
   OR COALESCE(d.cost, 0)          <> COALESCE(r.cost, 0)
ORDER BY ABS(COALESCE(d.request_count, 0) - COALESCE(r.request_count, 0)) DESC
LIMIT 100;
