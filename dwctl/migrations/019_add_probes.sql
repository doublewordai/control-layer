-- Create probes table
CREATE TABLE IF NOT EXISTS probes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL UNIQUE,
    deployment_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    interval_seconds INTEGER NOT NULL DEFAULT 60,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Create probe_results table
CREATE TABLE IF NOT EXISTS probe_results (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    probe_id UUID NOT NULL REFERENCES probes(id) ON DELETE CASCADE,
    executed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    success BOOLEAN NOT NULL,
    response_time_ms INTEGER,
    status_code INTEGER,
    error_message TEXT,
    response_data JSONB,
    metadata JSONB
);

-- Create indexes for efficient querying
CREATE INDEX IF NOT EXISTS idx_probe_results_probe_id ON probe_results(probe_id);
CREATE INDEX IF NOT EXISTS idx_probe_results_executed_at ON probe_results(executed_at);
CREATE INDEX IF NOT EXISTS idx_probe_results_success ON probe_results(success);
CREATE INDEX IF NOT EXISTS idx_probes_active ON probes(active);

-- Create function to update updated_at timestamp
CREATE OR REPLACE FUNCTION update_probes_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Create trigger to auto-update updated_at
CREATE TRIGGER update_probes_updated_at BEFORE UPDATE ON probes
    FOR EACH ROW EXECUTE FUNCTION update_probes_updated_at_column();
