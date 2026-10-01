-- Optional exact-name mappings only; primary model/class names need no rows.
SET LOCAL lock_timeout = '5s';

CREATE TABLE model_aliases (
    alias TEXT PRIMARY KEY,
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    serving_class_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT model_aliases_name_check CHECK (alias <> '' AND alias !~ '[[:space:]]'),
    CONSTRAINT model_aliases_model_class_fkey
        FOREIGN KEY (deployed_model_id, serving_class_id)
        REFERENCES model_serving_classes(deployed_model_id, id) ON DELETE RESTRICT
);

-- New empty table: covers model ownership and the composite class FK.
CREATE INDEX idx_model_aliases_model_class ON model_aliases (deployed_model_id, serving_class_id);

CREATE TRIGGER update_model_aliases_updated_at
    BEFORE UPDATE ON model_aliases
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

COMMENT ON TABLE model_aliases IS
    'Optional exact ingress synonyms mapping to canonical model/class identity. Primary names need no rows; no routing or discovery activation is implied.';
COMMENT ON COLUMN model_aliases.serving_class_id IS
    'Explicit class belonging to the same canonical model. Synonyms share its pricing, cache identity and usage attribution.';
COMMENT ON INDEX idx_model_aliases_model_class IS
    'Supports listing synonyms for a model/class and checking references before class removal.';
