-- The priority-decay top-up on the pending-counts query is gone: explicit
-- trailing windows on /monitoring/demand (completed rows counted by
-- completed_at, idx_requests_completed_trailing) express the same signal,
-- and this partial index served only the removed decay branch.
DROP INDEX IF EXISTS idx_requests_completed_flex_decay;
