-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_model_cache_tariffs_unique_active_org
    ON model_cache_tariffs (user_id, deployed_model_id, COALESCE(serving_class, '')) WHERE valid_until IS NULL AND user_id IS NOT NULL;
