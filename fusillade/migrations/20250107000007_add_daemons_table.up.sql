-- Add daemons table for tracking daemon lifecycle
CREATE TABLE daemons (
    id UUID PRIMARY KEY,
    status TEXT NOT NULL CHECK (status IN ('initializing', 'running', 'dead')),

    -- Immutable metadata
    hostname TEXT NOT NULL,
    pid INTEGER NOT NULL,
    version TEXT NOT NULL,
    config_snapshot JSONB NOT NULL,

    -- Lifecycle timestamps
    started_at TIMESTAMPTZ NOT NULL,
    last_heartbeat TIMESTAMPTZ NULL,
    stopped_at TIMESTAMPTZ NULL,

    -- Statistics (updated on heartbeat)
    requests_processed BIGINT NOT NULL DEFAULT 0,
    requests_failed BIGINT NOT NULL DEFAULT 0,
    requests_in_flight INTEGER NOT NULL DEFAULT 0,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- State constraints to ensure valid field combinations
    CONSTRAINT running_heartbeat_check CHECK ((status != 'running') OR (last_heartbeat IS NOT NULL)),
    CONSTRAINT dead_timestamp_check CHECK ((status != 'dead') OR (stopped_at IS NOT NULL))
);

-- Indexes for common queries
CREATE INDEX idx_daemons_status ON daemons(status);
CREATE INDEX idx_daemons_heartbeat ON daemons(last_heartbeat DESC) WHERE status = 'running';
CREATE INDEX idx_daemons_created_at ON daemons(created_at DESC);

-- Trigger to update updated_at timestamp
CREATE TRIGGER update_daemons_updated_at
    BEFORE UPDATE ON daemons
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
