-- Index for the archive movers' cancellation-grace exclusion.
--
-- `list_archivable_batches` excludes batches that still have canceled rows
-- which were in flight at cancel time (claimed_at IS NOT NULL) and were
-- canceled within the grace window — their billed results may still
-- supersede the cancel, so the batch must not move yet. The intended plan is
-- an idx_batches_archivable walk with a cheap per-candidate NOT EXISTS probe.
--
-- On prod-shaped data the planner instead DECORRELATES the anti-join: it
-- materializes every `state='canceled' AND claimed_at IS NOT NULL AND
-- canceled_at > now()-grace` row first — and with no index serving that
-- predicate, that is a full parallel seq scan of the ~100M-row requests
-- table, paid up front on EVERY mover tick regardless of LIMIT (measured
-- >90s per tick on a prod fork, 2026-07-20; caught by Gate 2 on staging).
--
-- This partial index contains only canceled rows that were claimed at cancel
-- time (normally a tiny, fast-expiring set), so whichever plan shape the
-- planner picks is instant: the decorrelated materialization becomes a
-- range scan on canceled_at over ~0 rows (measured: 8 buffer reads total),
-- and the correlated probe can use it too. Write amplification is
-- negligible — rows enter the index only on cancel-of-in-flight.
--
-- On large production tables, create this CONCURRENTLY before deploying so
-- the IF NOT EXISTS statement below becomes a no-op:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_cancel_grace
--   ON requests (canceled_at)
--   WHERE state = 'canceled' AND claimed_at IS NOT NULL;
--
-- (Build measured at ~6 min on a prod fork — non-concurrent creation would
-- write-lock requests for that long, hence the pre-create convention.)

CREATE INDEX IF NOT EXISTS idx_requests_cancel_grace
ON requests (canceled_at)
WHERE state = 'canceled' AND claimed_at IS NOT NULL;
