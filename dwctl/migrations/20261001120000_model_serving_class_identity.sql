-- Add storage only: existing model rows, routing and tariff resolution are unchanged.
-- Foreign-key creation briefly locks the referenced catalog tables.
SET LOCAL lock_timeout = '5s';

CREATE TABLE model_serving_classes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE RESTRICT,
    class_key TEXT NOT NULL CHECK (class_key ~ '^[a-z][a-z0-9_-]*$'),
    display_name TEXT NOT NULL CHECK (btrim(display_name) <> ''),
    inference_endpoint_id UUID NOT NULL REFERENCES inference_endpoints(id) ON DELETE RESTRICT,
    upstream_model_name TEXT NOT NULL CHECK (upstream_model_name <> '' AND upstream_model_name !~ '[[:space:]]'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT model_serving_classes_model_key_unique UNIQUE (deployed_model_id, class_key),
    -- Supports an alias FK that cannot bind a class belonging to another model.
    CONSTRAINT model_serving_classes_model_id_unique UNIQUE (deployed_model_id, id)
);

-- This index is built on a new, empty table in the same transaction.
CREATE INDEX idx_model_serving_classes_endpoint ON model_serving_classes (inference_endpoint_id);

CREATE TRIGGER update_model_serving_classes_updated_at
    BEFORE UPDATE ON model_serving_classes
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

COMMENT ON TABLE model_serving_classes IS
    'Stable service identities within an existing model, each with one default upstream. Empty until explicitly provisioned.';
COMMENT ON COLUMN model_serving_classes.class_key IS
    'Stable catalog key within the model; the initial offerings use standard and fast. Display names and upstream routing do not change class identity.';
COMMENT ON COLUMN model_serving_classes.inference_endpoint_id IS
    'Endpoint for the default upstream. Multiple classes may use the same endpoint and upstream model name.';
COMMENT ON INDEX idx_model_serving_classes_endpoint IS
    'Supports endpoint reference checks and finding classes affected by an endpoint change.';
