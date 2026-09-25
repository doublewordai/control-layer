-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_model_cache_tariffs_version_org
    ON model_cache_tariffs (user_id, deployed_model_id, valid_from, COALESCE(serving_class, '')) WHERE user_id IS NOT NULL;
