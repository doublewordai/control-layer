-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_model_tariffs_user_id ON model_tariffs (user_id) WHERE user_id IS NOT NULL;
