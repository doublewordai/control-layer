-- Provider request mappings for canonical OpenAI reasoning controls.
-- Endpoint configuration is the default; a deployment value overrides it.

ALTER TABLE inference_endpoints
    ADD COLUMN reasoning_translation JSONB;

ALTER TABLE deployed_models
    ADD COLUMN reasoning_translation JSONB;
