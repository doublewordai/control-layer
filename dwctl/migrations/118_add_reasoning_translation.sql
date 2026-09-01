-- Provider request mappings for canonical OpenAI reasoning controls.
-- Endpoint configuration is the default; deployment values can override it per API surface.

ALTER TABLE inference_endpoints
    ADD COLUMN reasoning_translation JSONB;

ALTER TABLE deployed_models
    ADD COLUMN reasoning_translation_overrides JSONB;

CREATE TRIGGER inference_endpoints_notify
AFTER INSERT OR UPDATE OR DELETE ON inference_endpoints
FOR EACH STATEMENT
EXECUTE FUNCTION notify_config_change();
