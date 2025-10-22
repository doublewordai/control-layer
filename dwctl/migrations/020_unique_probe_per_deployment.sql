-- Add unique constraint to ensure only one probe per deployment
ALTER TABLE probes ADD CONSTRAINT probes_deployment_id_unique UNIQUE (deployment_id);
