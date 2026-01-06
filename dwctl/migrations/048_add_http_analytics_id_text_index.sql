-- Add expression index for efficient joins between credits_transactions and http_analytics
--
-- The join `credits_transactions.source_id = http_analytics.id::text` requires a type cast
-- which prevents PostgreSQL from using the primary key index on http_analytics.id.
-- This expression index allows efficient nested loop joins for transaction queries.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_http_analytics_id_text
    ON http_analytics ((id::text));
