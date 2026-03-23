-- Add status_code to the covering index so usage queries filtering on
-- status_code BETWEEN 200 AND 299 can still use index-only scans.

DROP INDEX IF EXISTS idx_analytics_user_usage;

CREATE INDEX IF NOT EXISTS idx_analytics_user_usage
ON http_analytics (user_id, timestamp DESC)
INCLUDE (model, prompt_tokens, completion_tokens, total_cost, fusillade_batch_id, status_code)
WHERE user_id IS NOT NULL;
