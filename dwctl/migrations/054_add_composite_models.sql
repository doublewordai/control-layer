-- Add composite model support to deployed_models table
-- Composite models are virtual models that distribute requests across multiple
-- underlying deployed models based on configurable weights

-- Add composite model indicator and configuration columns to deployed_models
ALTER TABLE deployed_models ADD COLUMN is_composite BOOLEAN NOT NULL DEFAULT FALSE;

-- Load balancing strategy for composite models: weighted_random or priority
ALTER TABLE deployed_models ADD COLUMN lb_strategy VARCHAR DEFAULT 'weighted_random';

-- Fallback configuration for composite models
ALTER TABLE deployed_models ADD COLUMN fallback_enabled BOOLEAN DEFAULT TRUE;
ALTER TABLE deployed_models ADD COLUMN fallback_on_rate_limit BOOLEAN DEFAULT TRUE;
-- HTTP status codes that trigger fallback (e.g., [429, 500, 502, 503, 504])
ALTER TABLE deployed_models ADD COLUMN fallback_on_status INTEGER[] DEFAULT '{429, 500, 502, 503, 504}';

-- Add comments
COMMENT ON COLUMN deployed_models.is_composite IS 'Whether this is a composite model (virtual model distributing across multiple providers)';
COMMENT ON COLUMN deployed_models.lb_strategy IS 'Load balancing strategy for composite models: weighted_random (default) or priority';
COMMENT ON COLUMN deployed_models.fallback_enabled IS 'Whether to fall back to other providers when one fails (composite models only)';
COMMENT ON COLUMN deployed_models.fallback_on_rate_limit IS 'Fall back when provider is rate limited (composite models only)';
COMMENT ON COLUMN deployed_models.fallback_on_status IS 'HTTP status codes that trigger fallback to next provider (composite models only)';

-- Allow hosted_on to be NULL (required for composite models)
-- This drops the NOT NULL constraint on the hosted_on column
ALTER TABLE deployed_models ALTER COLUMN hosted_on DROP NOT NULL;

-- Constraint: composite models must NOT have hosted_on set, regular models MUST have it set
-- This CHECK constraint ensures data integrity: composite models have NULL hosted_on,
-- regular models have non-NULL hosted_on
ALTER TABLE deployed_models ADD CONSTRAINT deployed_models_composite_check
    CHECK (
        (is_composite = TRUE AND hosted_on IS NULL) OR
        (is_composite = FALSE AND hosted_on IS NOT NULL)
    );

-- Create the deployed_model_components junction table for composite model components
CREATE TABLE deployed_model_components (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The composite model that contains this component
    composite_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    -- The underlying deployed model that serves as a provider
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    -- Weight for load balancing (1-100, higher = more traffic)
    weight INTEGER NOT NULL DEFAULT 1 CHECK (weight >= 1 AND weight <= 100),
    -- Whether this component is active
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Each deployed model can only appear once per composite model
    UNIQUE (composite_model_id, deployed_model_id)
);

-- Add comments
COMMENT ON TABLE deployed_model_components IS 'Junction table linking composite models to their underlying deployed model components with weights';
COMMENT ON COLUMN deployed_model_components.composite_model_id IS 'The composite model (must be is_composite=true)';
COMMENT ON COLUMN deployed_model_components.deployed_model_id IS 'The underlying provider model (must be is_composite=false)';
COMMENT ON COLUMN deployed_model_components.weight IS 'Relative weight for load balancing (1-100). Higher weight = more traffic';
COMMENT ON COLUMN deployed_model_components.enabled IS 'Whether this component is active. Disabled components receive no traffic';

-- Indexes for performance
CREATE INDEX idx_deployed_model_components_composite_id ON deployed_model_components(composite_model_id);
CREATE INDEX idx_deployed_model_components_deployed_id ON deployed_model_components(deployed_model_id);

-- Constraint: ensure composite_model_id references a composite model
-- and deployed_model_id references a non-composite model
-- This is enforced via triggers since CHECK constraints can't reference other tables
CREATE OR REPLACE FUNCTION check_deployed_model_component_valid()
RETURNS TRIGGER AS $$
DECLARE
    composite_is_composite BOOLEAN;
    provider_is_composite BOOLEAN;
BEGIN
    -- Check that composite_model_id references a composite model
    SELECT is_composite INTO composite_is_composite
    FROM deployed_models WHERE id = NEW.composite_model_id;

    IF NOT composite_is_composite THEN
        RAISE EXCEPTION 'composite_model_id must reference a composite model (is_composite=true)';
    END IF;

    -- Check that deployed_model_id references a non-composite model
    SELECT is_composite INTO provider_is_composite
    FROM deployed_models WHERE id = NEW.deployed_model_id;

    IF provider_is_composite THEN
        RAISE EXCEPTION 'deployed_model_id must reference a regular model (is_composite=false)';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER deployed_model_components_validate
    BEFORE INSERT OR UPDATE ON deployed_model_components
    FOR EACH ROW
    EXECUTE FUNCTION check_deployed_model_component_valid();

-- Notify onwards config when components change
CREATE OR REPLACE FUNCTION notify_onwards_config_on_component_change()
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

CREATE TRIGGER deployed_model_components_notify_onwards
    AFTER INSERT OR UPDATE OR DELETE ON deployed_model_components
    FOR EACH ROW
    EXECUTE FUNCTION notify_onwards_config_on_component_change();
