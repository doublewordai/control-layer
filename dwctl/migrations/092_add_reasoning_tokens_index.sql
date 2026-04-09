-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_http_analytics_reasoning_tokens
ON http_analytics (reasoning_tokens)
WHERE reasoning_tokens > 0;
