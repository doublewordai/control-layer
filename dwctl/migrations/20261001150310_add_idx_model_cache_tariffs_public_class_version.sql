-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_model_cache_tariffs_public_class_version
    ON model_cache_tariffs (deployed_model_id, serving_class, valid_from) NULLS NOT DISTINCT
    WHERE user_id IS NULL;
