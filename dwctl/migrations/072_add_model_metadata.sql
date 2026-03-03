ALTER TABLE deployed_models
  ADD COLUMN metadata JSONB NOT NULL DEFAULT '{}';
