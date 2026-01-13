-- Add composite models support for weighted load balancing across multiple providers
-- A composite model is a virtual model that distributes requests across multiple
-- underlying deployed models based on configurable weights

-- Create the composite_models table
CREATE TABLE composite_models (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    alias VARCHAR NOT NULL,
    description TEXT,
    model_type VARCHAR,  -- Same as deployed_models: CHAT, EMBEDDINGS, RERANKER

    -- Rate limiting (same pattern as deployed_models)
    requests_per_second REAL,
    burst_size INTEGER,
    capacity INTEGER,
    batch_capacity INTEGER,

    -- Ownership and metadata
    created_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Add comments
COMMENT ON TABLE composite_models IS 'Virtual models that distribute requests across multiple underlying deployed models based on weights';
COMMENT ON COLUMN composite_models.alias IS 'User-facing model name (e.g., "gpt-4-balanced"). Must be unique across both composite_models and deployed_models aliases';
COMMENT ON COLUMN composite_models.model_type IS 'Model type: CHAT, EMBEDDINGS, or RERANKER';
COMMENT ON COLUMN composite_models.requests_per_second IS 'Global rate limit for this composite model';
COMMENT ON COLUMN composite_models.capacity IS 'Maximum concurrent requests for this composite model';

-- Create the composite_model_components junction table
CREATE TABLE composite_model_components (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    composite_model_id UUID NOT NULL REFERENCES composite_models(id) ON DELETE CASCADE,
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    weight INTEGER NOT NULL DEFAULT 1 CHECK (weight >= 1 AND weight <= 100),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Each deployed model can only appear once per composite model
    UNIQUE (composite_model_id, deployed_model_id)
);

-- Add comments
COMMENT ON TABLE composite_model_components IS 'Junction table linking composite models to their underlying deployed model components with weights';
COMMENT ON COLUMN composite_model_components.weight IS 'Relative weight for load balancing (1-100). Higher weight = more traffic';
COMMENT ON COLUMN composite_model_components.enabled IS 'Whether this component is active. Disabled components receive no traffic';

-- Create access control table for composite models (same pattern as deployment_groups)
CREATE TABLE composite_model_groups (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    composite_model_id UUID NOT NULL REFERENCES composite_models(id) ON DELETE CASCADE,
    group_id UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    granted_by UUID REFERENCES users(id),
    granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (composite_model_id, group_id)
);

COMMENT ON TABLE composite_model_groups IS 'Access control: which groups can use which composite models';

-- Indexes for performance
CREATE INDEX idx_composite_models_alias ON composite_models(alias);
CREATE INDEX idx_composite_models_created_by ON composite_models(created_by);
CREATE INDEX idx_composite_model_components_composite_id ON composite_model_components(composite_model_id);
CREATE INDEX idx_composite_model_components_deployed_id ON composite_model_components(deployed_model_id);
CREATE INDEX idx_composite_model_groups_composite_id ON composite_model_groups(composite_model_id);
CREATE INDEX idx_composite_model_groups_group_id ON composite_model_groups(group_id);

-- Ensure alias uniqueness across both tables
-- We need a function to check this since it spans two tables
CREATE OR REPLACE FUNCTION check_composite_alias_unique()
RETURNS TRIGGER AS $$
BEGIN
    -- Check if alias exists in deployed_models
    IF EXISTS (SELECT 1 FROM deployed_models WHERE alias = NEW.alias AND deleted = false) THEN
        RAISE EXCEPTION 'Alias "%" already exists in deployed_models', NEW.alias;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER composite_models_alias_unique
    BEFORE INSERT OR UPDATE OF alias ON composite_models
    FOR EACH ROW
    EXECUTE FUNCTION check_composite_alias_unique();

-- Also add a trigger to deployed_models to check against composite_models
CREATE OR REPLACE FUNCTION check_deployed_alias_not_composite()
RETURNS TRIGGER AS $$
BEGIN
    -- Only check if not being deleted
    IF NEW.deleted = false THEN
        IF EXISTS (SELECT 1 FROM composite_models WHERE alias = NEW.alias) THEN
            RAISE EXCEPTION 'Alias "%" already exists in composite_models', NEW.alias;
        END IF;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER deployed_models_alias_not_composite
    BEFORE INSERT OR UPDATE OF alias, deleted ON deployed_models
    FOR EACH ROW
    EXECUTE FUNCTION check_deployed_alias_not_composite();

-- Notify onwards config when composite models change
CREATE OR REPLACE FUNCTION notify_onwards_config_on_composite_change()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed', json_build_object(
        'table', TG_TABLE_NAME,
        'operation', TG_OP,
        'id', COALESCE(NEW.id::text, OLD.id::text),
        'timestamp', (extract(epoch from clock_timestamp()) * 1000000)::bigint
    )::text);
    RETURN COALESCE(NEW, OLD);
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER composite_models_notify_onwards
    AFTER INSERT OR UPDATE OR DELETE ON composite_models
    FOR EACH ROW
    EXECUTE FUNCTION notify_onwards_config_on_composite_change();

CREATE TRIGGER composite_model_components_notify_onwards
    AFTER INSERT OR UPDATE OR DELETE ON composite_model_components
    FOR EACH ROW
    EXECUTE FUNCTION notify_onwards_config_on_composite_change();

CREATE TRIGGER composite_model_groups_notify_onwards
    AFTER INSERT OR UPDATE OR DELETE ON composite_model_groups
    FOR EACH ROW
    EXECUTE FUNCTION notify_onwards_config_on_composite_change();
