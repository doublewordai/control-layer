-- Only relax scope checks after replacement uniqueness guards are validated.
SET LOCAL lock_timeout = '5s';
ALTER TABLE model_tariffs DROP CONSTRAINT model_tariffs_class_scope;
ALTER TABLE model_tariffs ADD CONSTRAINT model_tariffs_class_scope
    CHECK (serving_class IS NULL OR serving_class ~ '^[a-z][a-z0-9_-]*$') NOT VALID;
ALTER TABLE model_cache_tariffs DROP CONSTRAINT model_cache_tariffs_class_scope;
ALTER TABLE model_cache_tariffs ADD CONSTRAINT model_cache_tariffs_class_scope
    CHECK (serving_class IS NULL OR serving_class ~ '^[a-z][a-z0-9_-]*$') NOT VALID;
