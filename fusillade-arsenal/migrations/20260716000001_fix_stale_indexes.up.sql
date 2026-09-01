-- Two index fixes riding the Phase 3 schema-expand PR. Evidence for both:
-- prod baseline capture 2026-07-15 (fusillade-phase3-baseline-metrics.md in
-- the clay/core workspace root).

-- ----------------------------------------------------------------------------
-- 1. Drop idx_requests_active_first_sort: superseded eight days after its
--    creation by idx_requests_active_first_tier (20260424000000), which adds
--    the service_tier dimension the queries grew. The planner has used the
--    _tier index ever since (prod: 3,918 scans vs 0 for this one) — this is
--    4.1 GB of pure leftover write amplification on every requests insert.
-- ----------------------------------------------------------------------------
DROP INDEX IF EXISTS idx_requests_active_first_sort;

-- ----------------------------------------------------------------------------
-- 2. Re-predicate idx_batches_pending_notification to match the query it was
--    built for. The freeze/notification poller (poll_completed_batches
--    candidates CTE) deliberately does NOT filter on terminal timestamps —
--    its whole job is finding batches that are terminal BY COUNT but not yet
--    stamped. The old index REQUIRED a terminal timestamp in its predicate,
--    so it could never serve that query: prod showed 0 scans on the index
--    and a full ~800k-row seq scan of batches per poll (~587 ms, every few
--    seconds — 85.6B tuples read since stat reset).
--
--    The new predicate is exactly the poller's static conditions, so the
--    index contains only not-yet-notified live batches (a handful of rows).
--    If the poller's WHERE changes, this predicate must change WITH it —
--    an index predicate must be implied by the query's, or the planner
--    silently falls back to seq scans (which is precisely what happened).
--
--    Plain CREATE INDEX (not CONCURRENTLY — migrations run in a transaction):
--    one scan of batches at boot, seconds.
-- ----------------------------------------------------------------------------
DROP INDEX IF EXISTS idx_batches_pending_notification;
CREATE INDEX idx_batches_pending_notification
    ON batches (id)
    WHERE notification_sent_at IS NULL
      AND deleted_at IS NULL
      AND cancelling_at IS NULL
      AND total_requests > 0;

COMMENT ON INDEX idx_batches_pending_notification IS
    'Serves the freeze/notification poller (poll_completed_batches candidate '
    'scan). Predicate must stay implied by that query''s WHERE — it '
    'intentionally has NO terminal-timestamp condition because the poller '
    'finds terminal-by-count-but-unstamped batches. The previous version '
    'required terminal timestamps and was therefore never used (0 scans; '
    'every poll seq-scanned all of batches).';
