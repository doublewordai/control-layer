-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_model_cache_tariffs_version_general
    ON model_cache_tariffs (deployed_model_id, valid_from) WHERE user_id IS NULL;
