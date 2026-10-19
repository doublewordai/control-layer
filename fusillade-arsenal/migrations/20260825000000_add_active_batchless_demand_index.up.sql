-- Index for the batchless branch of get_pending_request_counts_by_model_*
-- (the forward windows on /monitoring/demand).
--
-- `batchless_counts` counts daemon-claimable rows with no parent batch
-- (flex/priority async). Its only usable index was idx_requests_state, a plain
-- btree on `state`: the planner descended it for
-- state IN ('pending','claimed','processing'), heap-fetched every matching row
-- to read `batch_id`, `template_id` and `service_tier`, and then discarded
-- nearly all of them.
--
-- That makes the branch cost proportional to the entire active queue while the
-- answer depends only on the batchless subset — and those two quantities are
-- unrelated. A deployment with a large batch backlog and almost no batchless
-- traffic pays the full scan to return almost nothing, which is precisely the
-- shape that broke: the branch dominated the demand query until it exceeded
-- its statement_timeout on every execution. That is not a degraded response —
-- scouter's control loop aborts when the demand fetch fails, so it stops
-- planning altogether: no replica changes, no model_filters writes.
--
-- This index inverts the relationship. The predicate carries the whole filter
-- and the key columns carry everything the CTE projects, so the branch becomes
-- an Index Only Scan over just the batchless working set and its cost tracks
-- that subset instead of the queue:
--
--   batchless branch   ~73 s -> sub-millisecond
--   whole demand query  >60 s -> under 10 s
--
-- (measured against a copy of a production-sized dataset exhibiting the above)
--
-- The residual is the batched branch, which no index removes: counting active
-- batched requests genuinely requires reading them.
--
-- Size is bounded by construction, and deliberately so: the predicate excludes
-- every terminal state, so rows leave this index as soon as they complete or
-- fail. It tracks the active batchless working set and never the archive,
-- which keeps it orders of magnitude smaller than the trailing-demand indexes
-- (idx_requests_completed_trailing / idx_requests_failed_trailing) that must
-- cover every terminal row.
--
-- `service_tier` is a key column rather than an INCLUDE: the CTE groups by it,
-- and keeping it in the key lets the scan stay index-only. `created_at` is
-- last because the CTE filters on a derived deadline
-- (created_at + completion window), which is not directly indexable — it is
-- carried to avoid a heap fetch, not to be seeked on.
--
-- On large production tables, create this CONCURRENTLY before deploying the
-- migration so the IF NOT EXISTS statement below becomes a no-op:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_batchless_demand
--   ON requests (model, service_tier, created_at)
--   WHERE state IN ('pending', 'claimed', 'processing')
--     AND batch_id IS NULL
--     AND template_id IS NOT NULL
--     AND service_tier IS DISTINCT FROM 'background';
--
-- Caveat for future readers: the planner's row estimate for this index is
-- poor, because partial-index selectivity is derived from table-level
-- statistics — expect an estimate orders of magnitude above the true row
-- count. It still chooses the index (the cost gap is far too wide for the
-- estimate to matter), but do not be alarmed by it in an EXPLAIN.

CREATE INDEX IF NOT EXISTS idx_requests_active_batchless_demand
ON requests (model, service_tier, created_at)
WHERE state IN ('pending', 'claimed', 'processing')
  AND batch_id IS NULL
  AND template_id IS NOT NULL
  AND service_tier IS DISTINCT FROM 'background';
