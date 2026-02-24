-- Add trusted and open_responses_adapter columns to deployed_models table.
-- When strict mode is enabled in onwards, trusted marks providers as trusted
-- and bypasses response sanitization for them.
-- open_responses_adapter enables the onwards adapter that converts /v1/responses
-- requests to /v1/chat/completions for providers that don't natively support
-- the OpenAI Responses API.

ALTER TABLE deployed_models
ADD COLUMN trusted BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN deployed_models.trusted IS
  'Mark provider as trusted in strict mode (bypasses sanitization). Only used when onwards.strict_mode=true. Default: false';

ALTER TABLE deployed_models
ADD COLUMN open_responses_adapter BOOLEAN DEFAULT NULL;

COMMENT ON COLUMN deployed_models.open_responses_adapter IS
  'Enable the onwards adapter that converts /v1/responses to /v1/chat/completions. NULL means use the Rust default (true). Only meaningful when onwards.strict_mode=true.';
