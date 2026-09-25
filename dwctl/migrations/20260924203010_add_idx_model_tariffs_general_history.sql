-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_model_tariffs_general_history
    ON model_tariffs (deployed_model_id) WHERE user_id IS NULL;
