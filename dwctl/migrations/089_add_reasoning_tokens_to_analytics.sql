ALTER TABLE http_analytics
ADD COLUMN reasoning_tokens BIGINT NOT NULL DEFAULT 0;

CREATE INDEX idx_http_analytics_reasoning_tokens
ON http_analytics (reasoning_tokens)
WHERE reasoning_tokens > 0;
