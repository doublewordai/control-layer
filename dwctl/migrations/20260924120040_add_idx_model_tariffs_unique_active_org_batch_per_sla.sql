-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_model_tariffs_unique_active_org_batch_per_sla
    ON model_tariffs (user_id, deployed_model_id, api_key_purpose, completion_window, COALESCE(serving_class, ''))
    WHERE valid_until IS NULL AND user_id IS NOT NULL AND api_key_purpose = 'batch' AND completion_window IS NOT NULL;
