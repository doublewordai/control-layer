-- Add model_tariffs table to support multiple pricing tiers per deployed model
-- Migrate existing upstream pricing to 'batch' tariff
-- Supports temporal validity to ensure accurate historical chargeback

-- Create the model_tariffs table
CREATE TABLE model_tariffs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    input_price_per_token DECIMAL(12, 8) NOT NULL DEFAULT 0,
    output_price_per_token DECIMAL(12, 8) NOT NULL DEFAULT 0,
    is_default BOOLEAN NOT NULL DEFAULT false,
    valid_from TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Ensure tariff names are unique per model
    CONSTRAINT model_tariffs_model_name_unique UNIQUE (deployed_model_id, name)
);

-- Unique partial index ensures only one active tariff with each name per model
-- Also optimizes queries for current tariffs (WHERE deployed_model_id = X AND valid_until IS NULL)
-- This is the primary index - historical tariff queries can use sequential scan (rare operation)
CREATE UNIQUE INDEX idx_model_tariffs_unique_active ON model_tariffs(deployed_model_id, name) WHERE valid_until IS NULL;

-- Add comments
COMMENT ON TABLE model_tariffs IS 'Pricing tariffs for deployed models. Each model can have multiple tariffs (e.g., batch, realtime, premium). Supports temporal validity for accurate historical chargeback.';
COMMENT ON COLUMN model_tariffs.name IS 'Tariff name (e.g., "batch", "realtime", "premium")';
COMMENT ON COLUMN model_tariffs.input_price_per_token IS 'Price per input token';
COMMENT ON COLUMN model_tariffs.output_price_per_token IS 'Price per output token';
COMMENT ON COLUMN model_tariffs.is_default IS 'Whether this is the default tariff for the model';
COMMENT ON COLUMN model_tariffs.valid_from IS 'Timestamp when this tariff becomes valid. Used for historical price lookups to ensure accurate chargeback.';
COMMENT ON COLUMN model_tariffs.valid_until IS 'Timestamp when this tariff expires. NULL means currently active. Set this when creating a new version of a tariff.';

-- Migrate existing upstream pricing to 'batch' tariff
-- Only create tariff rows for models that have pricing set
INSERT INTO model_tariffs (deployed_model_id, name, input_price_per_token, output_price_per_token, is_default)
SELECT
    id as deployed_model_id,
    'batch' as name,
    COALESCE(upstream_input_price_per_token, 0) as input_price_per_token,
    COALESCE(upstream_output_price_per_token, 0) as output_price_per_token,
    true as is_default
FROM deployed_models
WHERE upstream_input_price_per_token IS NOT NULL
   OR upstream_output_price_per_token IS NOT NULL;

-- Add trigger to update updated_at timestamp
CREATE TRIGGER update_model_tariffs_updated_at
    BEFORE UPDATE ON model_tariffs
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Notify onwards config when tariffs change
CREATE OR REPLACE FUNCTION notify_onwards_config_on_tariff_change()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed', json_build_object(
        'table', TG_TABLE_NAME,
        'operation', TG_OP,
        'id', COALESCE(NEW.id::text, OLD.id::text)
    )::text);
    RETURN COALESCE(NEW, OLD);
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER model_tariffs_notify_onwards
    AFTER INSERT OR UPDATE OR DELETE ON model_tariffs
    FOR EACH ROW
    EXECUTE FUNCTION notify_onwards_config_on_tariff_change();

-- Drop the old pricing columns (after data migration)
ALTER TABLE deployed_models
DROP COLUMN upstream_input_price_per_token,
DROP COLUMN upstream_output_price_per_token;
