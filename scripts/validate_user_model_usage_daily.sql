-- Validate user_model_usage_daily against the old user_model_usage rollup before the
-- read cutover (COR-506). Both are additive sums over the same 2xx http_analytics rows
-- (daily = per-day; old = all-time), so once both cursors have caught up their per
-- (user, model) totals must match.
--
-- Expected output: zero rows, OR a handful of rows with tiny diffs on currently-active
-- users — that's the in-flight tail (rows one cursor has folded in and the other hasn't
-- yet). Large diffs (thousands of requests, or whole (user, model) pairs missing on one
-- side) indicate a real problem — do NOT cut reads over until they're understood.
--
-- Run against the same DB after the backfill has finished and the daemon has drained.

WITH daily AS (
    SELECT user_id, model,
           SUM(input_tokens)  AS input_tokens,
           SUM(output_tokens) AS output_tokens,
           SUM(cost)          AS cost,
           SUM(request_count) AS request_count
    FROM user_model_usage_daily
    GROUP BY user_id, model
)
SELECT
    COALESCE(d.user_id, o.user_id)          AS user_id,
    COALESCE(d.model, o.model)              AS model,
    o.request_count                         AS old_requests,
    d.request_count                         AS daily_requests,
    COALESCE(d.request_count, 0) - COALESCE(o.request_count, 0) AS request_diff,
    COALESCE(d.input_tokens, 0)  - COALESCE(o.input_tokens, 0)  AS input_token_diff,
    COALESCE(d.output_tokens, 0) - COALESCE(o.output_tokens, 0) AS output_token_diff,
    COALESCE(d.cost, 0)          - COALESCE(o.cost, 0)          AS cost_diff
FROM daily d
FULL OUTER JOIN user_model_usage o USING (user_id, model)
WHERE COALESCE(d.request_count, 0) <> COALESCE(o.request_count, 0)
   OR COALESCE(d.input_tokens, 0)  <> COALESCE(o.input_tokens, 0)
   OR COALESCE(d.output_tokens, 0) <> COALESCE(o.output_tokens, 0)
   OR COALESCE(d.cost, 0)          <> COALESCE(o.cost, 0)
ORDER BY ABS(COALESCE(d.request_count, 0) - COALESCE(o.request_count, 0)) DESC
LIMIT 100;
