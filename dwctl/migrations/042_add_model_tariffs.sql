-- Add model_tariffs table to support multiple pricing tiers per deployed model
-- Migrate existing upstream pricing to 'realtime' tariff so we always have a fallback
-- Supports temporal validity to ensure accurate historical chargeback

-- Create the model_tariffs table
CREATE TABLE model_tariffs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployed_model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    input_price_per_token DECIMAL(12, 8) NOT NULL DEFAULT 0,
    output_price_per_token DECIMAL(12, 8) NOT NULL DEFAULT 0,
    valid_from TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    valid_until TIMESTAMPTZ,
    api_key_purpose VARCHAR(50)
);

-- Add comments
COMMENT ON TABLE model_tariffs IS 'Pricing tariffs for deployed models per API key purpose. Each model can have different pricing for realtime, batch, and playground usage. Supports temporal validity for accurate historical chargeback.';
COMMENT ON COLUMN model_tariffs.name IS 'Descriptive name for the tariff (e.g., "Standard Pricing", "Premium Tier"). Purely informational.';
COMMENT ON COLUMN model_tariffs.input_price_per_token IS 'Price per input token';
COMMENT ON COLUMN model_tariffs.output_price_per_token IS 'Price per output token';
COMMENT ON COLUMN model_tariffs.valid_from IS 'Timestamp when this tariff becomes valid. Used for historical price lookups to ensure accurate chargeback.';
COMMENT ON COLUMN model_tariffs.valid_until IS 'Timestamp when this tariff expires. NULL means currently active. Set this when creating a new version of a tariff.';
COMMENT ON COLUMN model_tariffs.api_key_purpose IS 'API key purpose this tariff applies to (realtime, batch, playground). Each active tariff must specify a purpose.';

-- Migrate existing upstream pricing to 'realtime' tariff
-- Only create tariff rows for models that have pricing set
INSERT INTO model_tariffs (deployed_model_id, name, input_price_per_token, output_price_per_token, api_key_purpose)
SELECT
    id as deployed_model_id,
    'realtime_imported' as name,
    COALESCE(upstream_input_price_per_token, 0) as input_price_per_token,
    COALESCE(upstream_output_price_per_token, 0) as output_price_per_token,
    'realtime' as api_key_purpose
FROM deployed_models
WHERE upstream_input_price_per_token IS NOT NULL
   OR upstream_output_price_per_token IS NOT NULL;


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

-- Create unique constraint: max one active tariff per (model, purpose) combination
-- Only applies to tariffs WITH a purpose (WHERE api_key_purpose IS NOT NULL)
-- Tariffs without a purpose have no uniqueness constraint
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_per_purpose
    ON model_tariffs(deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND api_key_purpose IS NOT NULL;

-- Drop the old pricing columns (after data migration)
ALTER TABLE deployed_models
DROP COLUMN upstream_input_price_per_token,
DROP COLUMN upstream_output_price_per_token;
