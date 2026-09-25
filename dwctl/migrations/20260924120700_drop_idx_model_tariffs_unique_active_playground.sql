-- no-transaction
-- The scoped replacement has been validated; general-price uniqueness remains enforced.
DROP INDEX CONCURRENTLY IF EXISTS idx_model_tariffs_unique_active_playground;
