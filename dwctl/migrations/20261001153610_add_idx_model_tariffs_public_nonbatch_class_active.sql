-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_model_tariffs_public_nonbatch_class_active
    ON model_tariffs (deployed_model_id, serving_class, api_key_purpose) NULLS NOT DISTINCT
    WHERE user_id IS NULL AND valid_until IS NULL AND api_key_purpose <> 'batch';
