-- Add pricing support for deployed models

-- Add pricing columns to deployed_models table
ALTER TABLE deployed_models
-- User-facing pricing (always per-token)
ADD COLUMN upstream_input_price_per_token DECIMAL(12, 8) DEFAULT NULL,
ADD COLUMN upstream_output_price_per_token DECIMAL(12, 8) DEFAULT NULL,
-- Provider pricing (flexible)
ADD COLUMN downstream_pricing_mode VARCHAR DEFAULT NULL,
ADD COLUMN downstream_input_price_per_token DECIMAL(12, 8) DEFAULT NULL,
ADD COLUMN downstream_output_price_per_token DECIMAL(12, 8) DEFAULT NULL,
ADD COLUMN downstream_hourly_rate DECIMAL(10, 2) DEFAULT NULL,
ADD COLUMN downstream_input_token_cost_ratio DECIMAL(3, 2) DEFAULT NULL;

-- Add comments for clarity
COMMENT ON COLUMN deployed_models.upstream_input_price_per_token IS 'User-facing cost per input token';
COMMENT ON COLUMN deployed_models.upstream_output_price_per_token IS 'User-facing cost per output token';
COMMENT ON COLUMN deployed_models.downstream_pricing_mode IS 'Provider pricing type: "per_token", "hourly", or NULL';
COMMENT ON COLUMN deployed_models.downstream_input_price_per_token IS 'Provider cost per input token (only when downstream_pricing_mode = "per_token")';
COMMENT ON COLUMN deployed_models.downstream_output_price_per_token IS 'Provider cost per output token (only when downstream_pricing_mode = "per_token")';
COMMENT ON COLUMN deployed_models.downstream_hourly_rate IS 'Provider hourly rate (only when downstream_pricing_mode = "hourly")';
COMMENT ON COLUMN deployed_models.downstream_input_token_cost_ratio IS 'Proportion of hourly cost for input tokens (0.0-1.0, only when downstream_pricing_mode = "hourly")';

-- Add constraint to ensure input_token_cost_ratio is between 0 and 1 when not null
ALTER TABLE deployed_models
ADD CONSTRAINT check_downstream_input_token_cost_ratio
CHECK (downstream_input_token_cost_ratio IS NULL OR (downstream_input_token_cost_ratio >= 0.0 AND downstream_input_token_cost_ratio <= 1.0));
