-- Add throughput column for SLA capacity management
-- throughput is in requests/second and used to calculate if we can accept new batches
-- within their SLA window.
-- NULL means use the default global config value for the model at runtime (for backward compatibility)
-- it is recommended to set real values for better capacity planning.

ALTER TABLE deployed_models
ADD COLUMN throughput REAL NULL;

COMMENT ON COLUMN deployed_models.throughput IS 
'Model throughput in requests/second for batch SLA capacity calculations. NULL means use config default.';