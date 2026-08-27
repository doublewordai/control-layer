-- Covering index for the batched branch of
-- get_pending_request_counts_by_model_window_and_tier (the `active_request_counts`
-- CTE), which serves both /monitoring/demand and batch admission
-- (`reserve_capacity` in dwctl).
--
-- That branch counts active batched rows grouped by (batch_id, model,
-- service_tier), optionally restricted to a model list. The existing partial
-- indexes (idx_requests_active_non_priority_counts / idx_requests_active_sla_counts,
-- both keyed (batch_id, model)) find the rows but cannot serve the CTE
-- index-only: `service_tier` is neither a key nor an INCLUDE column, so every
-- matching row is heap-fetched to read it. With a large batch backlog that is
-- a heap fetch per active row — measured at ~4-7 s for ~4.9M active rows on a
-- production-sized dataset, and the same query was observed at 22-59 s on the
-- production primary while it was contended by claim_requests.
--
-- This index carries every column the CTE projects or filters on (model,
-- service_tier, batch_id) and leads with `model`, so a per-model admission
-- check seeks straight to that model's active rows and the whole branch runs
-- as an Index Only Scan. On the same dataset:
--
--   admission (one model, 24h):  6.5 s -> 1.1-1.5 s
--   demand    (all models, 24h): 4.0 s -> 1.1-1.5 s
--
-- Like idx_requests_active_batchless_demand, the predicate excludes every
-- terminal state, so the index tracks only the active working set (36 MB at
-- ~4.9M active rows) and never the archive.
--
-- It also supersedes the two (batch_id, model) partial indexes that previously
-- served this branch, so they are dropped below:
--
--   idx_requests_active_sla_counts (20260722): production pg_stat_user_indexes
--     showed it used by the admission variant only (its last scan coincides
--     with the last admission timeout before the gate was disabled); with the
--     new index present the planner prefers the index-only path for that shape.
--   idx_requests_active_non_priority_counts (20260528): same key with a looser
--     predicate; the planner never chose it over the 20260722 index (2 scans
--     in total on production).
--
-- The claim queries do not use either (they run on idx_requests_active_with_template
-- and the pending_* partials), verified with EXPLAIN ANALYZE against a copy of
-- production with both indexes dropped.
--
-- On large production tables, create this CONCURRENTLY before deploying the
-- migration so the IF NOT EXISTS statement below becomes a no-op:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_batched_demand
--   ON requests (model, service_tier, batch_id)
--   WHERE state IN ('pending', 'claimed', 'processing')
--     AND batch_id IS NOT NULL
--     AND template_id IS NOT NULL
--     AND service_tier IS DISTINCT FROM 'background';

CREATE INDEX IF NOT EXISTS idx_requests_active_batched_demand
ON requests (model, service_tier, batch_id)
WHERE state IN ('pending', 'claimed', 'processing')
  AND batch_id IS NOT NULL
  AND template_id IS NOT NULL
  AND service_tier IS DISTINCT FROM 'background';

DROP INDEX IF EXISTS idx_requests_active_sla_counts;
DROP INDEX IF EXISTS idx_requests_active_non_priority_counts;
