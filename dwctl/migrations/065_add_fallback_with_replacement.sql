-- Add with-replacement and max-attempts options for weighted random failover.
-- Both default to NULL which preserves existing behaviour (without replacement,
-- attempt count = provider count).

ALTER TABLE deployed_models
  ADD COLUMN fallback_with_replacement BOOLEAN DEFAULT FALSE,
  ADD COLUMN fallback_max_attempts INTEGER DEFAULT NULL;
