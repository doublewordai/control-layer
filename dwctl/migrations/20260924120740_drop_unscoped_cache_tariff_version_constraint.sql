-- Constraint-owned indexes cannot be dropped concurrently. The scoped replacement
-- indexes are already valid; this is a bounded metadata-only constraint removal.
SET LOCAL lock_timeout = '5s';
ALTER TABLE model_cache_tariffs DROP CONSTRAINT IF EXISTS model_cache_tariffs_deployed_model_id_valid_from_key;
