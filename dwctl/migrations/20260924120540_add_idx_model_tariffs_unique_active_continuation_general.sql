-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_model_tariffs_unique_active_continuation_general
    ON model_tariffs (deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND user_id IS NULL AND api_key_purpose = 'continuation';
