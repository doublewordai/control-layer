-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_model_cache_tariffs_unique_active_general
    ON model_cache_tariffs (deployed_model_id)
    WHERE valid_until IS NULL AND user_id IS NULL;
