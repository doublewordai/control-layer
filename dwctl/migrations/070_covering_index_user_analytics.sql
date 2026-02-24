-- Replace idx_analytics_user_id with a covering index for the usage date-range queries.
--
-- The old index had the same key columns (user_id, timestamp DESC) but no INCLUDE
-- columns. Because user rows are scattered across the table (correlation -0.24),
-- the planner refused to use it — fetching each row from the heap meant random I/O
-- that was slower than just scanning by timestamp and filtering.
--
-- Adding INCLUDE columns lets PostgreSQL answer queries entirely from the index
-- (Index Only Scan), bypassing the heap. 30-day usage queries: ~35s → ~53ms.

DROP INDEX IF EXISTS idx_analytics_user_id;

CREATE INDEX IF NOT EXISTS idx_analytics_user_usage
ON http_analytics (user_id, timestamp DESC)
INCLUDE (model, prompt_tokens, completion_tokens, total_cost, fusillade_batch_id)
WHERE user_id IS NOT NULL;
