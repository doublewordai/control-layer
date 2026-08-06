-- Add component groups to composite models.
-- A group is a named sub-pool inside a composite model with its own load
-- balancing strategy. Direct components (group_id IS NULL) and groups share
-- one sort-order sequence at the composite root; members of a group have
-- their own per-group sort-order sequence.

CREATE TABLE deployed_model_component_groups (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The composite model that contains this group
    composite_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    name VARCHAR NOT NULL,
    -- Load balancing strategy applied among the group's members: weighted_random or priority
    lb_strategy VARCHAR NOT NULL DEFAULT 'weighted_random',
    -- Weight of the group within the composite root sequence (1-100)
    weight INTEGER NOT NULL DEFAULT 1 CHECK (weight >= 1 AND weight <= 100),
    -- Position within the composite root sequence (shared with direct components)
    sort_order INTEGER NOT NULL DEFAULT 0,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (composite_model_id, name)
);

COMMENT ON TABLE deployed_model_component_groups IS 'Named sub-pools inside composite models, each with its own load balancing strategy';
COMMENT ON COLUMN deployed_model_component_groups.composite_model_id IS 'The composite model (must be is_composite=true)';
COMMENT ON COLUMN deployed_model_component_groups.lb_strategy IS 'Load balancing strategy among group members: weighted_random (default) or priority';
COMMENT ON COLUMN deployed_model_component_groups.weight IS 'Relative weight of the group in the composite root sequence (1-100)';
COMMENT ON COLUMN deployed_model_component_groups.sort_order IS 'Position in the composite root sequence, shared with direct components';

CREATE INDEX idx_deployed_model_component_groups_composite_id ON deployed_model_component_groups(composite_model_id);

-- Components can now belong to a group. NULL = direct member of the composite
-- (existing behaviour, unchanged). Deleting a group promotes its members to
-- direct components rather than deleting them.
ALTER TABLE deployed_model_components
    ADD COLUMN group_id UUID REFERENCES deployed_model_component_groups(id) ON DELETE SET NULL;

COMMENT ON COLUMN deployed_model_components.group_id IS 'Optional group this component belongs to. NULL = direct member of the composite';

CREATE INDEX idx_deployed_model_components_group_id ON deployed_model_components(group_id);

-- Constraint: composite_model_id must reference a composite model.
-- Enforced via trigger since CHECK constraints can't reference other tables
-- (mirrors check_deployed_model_component_valid from migration 054).
CREATE OR REPLACE FUNCTION check_deployed_model_component_group_valid()
RETURNS TRIGGER AS $$
DECLARE
    composite_is_composite BOOLEAN;
BEGIN
    SELECT is_composite INTO composite_is_composite
    FROM deployed_models WHERE id = NEW.composite_model_id;

    IF NOT composite_is_composite THEN
        RAISE EXCEPTION 'composite_model_id must reference a composite model (is_composite=true)';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER deployed_model_component_groups_validate
    BEFORE INSERT OR UPDATE ON deployed_model_component_groups
    FOR EACH ROW
    EXECUTE FUNCTION check_deployed_model_component_group_valid();

-- Constraint: a component's group must belong to the same composite model.
CREATE OR REPLACE FUNCTION check_deployed_model_component_group_matches()
RETURNS TRIGGER AS $$
DECLARE
    group_composite_id UUID;
BEGIN
    IF NEW.group_id IS NOT NULL THEN
        SELECT composite_model_id INTO group_composite_id
        FROM deployed_model_component_groups WHERE id = NEW.group_id;

        IF group_composite_id IS DISTINCT FROM NEW.composite_model_id THEN
            RAISE EXCEPTION 'group_id must reference a group of the same composite model';
        END IF;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER deployed_model_components_group_matches
    BEFORE INSERT OR UPDATE ON deployed_model_components
    FOR EACH ROW
    EXECUTE FUNCTION check_deployed_model_component_group_matches();

-- Notify onwards config when groups change (same channel/function as components,
-- from migration 054).
CREATE TRIGGER deployed_model_component_groups_notify_onwards
    AFTER INSERT OR UPDATE OR DELETE ON deployed_model_component_groups
    FOR EACH ROW
    EXECUTE FUNCTION notify_onwards_config_on_component_change();
