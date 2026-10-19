-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_analytics_custom_id
    ON http_analytics (custom_id)
    WHERE custom_id IS NOT NULL;
