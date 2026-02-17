-- Add trusted column to deployed_models table
-- When strict mode is enabled in onwards, this flag marks providers as trusted
-- and bypasses response sanitization for them

ALTER TABLE deployed_models
ADD COLUMN trusted BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN deployed_models.trusted IS
  'Mark provider as trusted in strict mode (bypasses sanitization). Only used when onwards.strict_mode=true. Default: false';
