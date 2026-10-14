-- no-transaction
-- Reverting returns the throughput read to scanning requests.
DROP INDEX CONCURRENTLY IF EXISTS idx_requests_tolerated_completions;
