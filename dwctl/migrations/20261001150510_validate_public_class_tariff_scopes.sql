SET LOCAL lock_timeout = '5s';
ALTER TABLE model_tariffs VALIDATE CONSTRAINT model_tariffs_class_scope;
ALTER TABLE model_cache_tariffs VALIDATE CONSTRAINT model_cache_tariffs_class_scope;
