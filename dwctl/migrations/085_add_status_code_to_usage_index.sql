-- Add status_code to the covering index so usage queries filtering on
-- status_code BETWEEN 200 AND 299 can still use index-only scans.
-- Creates a new index alongside the old one (which can be dropped separately).

CREATE INDEX IF NOT EXISTS idx_analytics_user_status_ok_usage
ON http_analytics (user_id, timestamp DESC)
INCLUDE (model, prompt_tokens, completion_tokens, total_cost, fusillade_batch_id, status_code)
WHERE user_id IS NOT NULL;
