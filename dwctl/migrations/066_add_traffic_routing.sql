-- Per-model traffic routing rules with referential integrity.
-- Redirect targets use FK to deployed_models(id) so alias renames
-- and model deletions are handled at the DB level.
CREATE TABLE model_traffic_rules (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    api_key_purpose VARCHAR NOT NULL,
    action VARCHAR NOT NULL,
    redirect_target_id UUID REFERENCES deployed_models(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT valid_action CHECK (
        (action = 'deny' AND redirect_target_id IS NULL) OR
        (action = 'redirect' AND redirect_target_id IS NOT NULL)
    ),
    CONSTRAINT unique_purpose_per_model UNIQUE (deployed_model_id, api_key_purpose)
);

-- Trigger LISTEN/NOTIFY on traffic rule changes (uses existing function from 049)
CREATE TRIGGER model_traffic_rules_notify
    AFTER INSERT OR UPDATE OR DELETE ON model_traffic_rules
    FOR EACH STATEMENT EXECUTE FUNCTION notify_config_change();

-- Per-model allowed batch completion windows (NULL = use global default)
ALTER TABLE deployed_models
ADD COLUMN IF NOT EXISTS allowed_batch_completion_windows TEXT[];
