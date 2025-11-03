-- Create requests table for the batcher system
CREATE TABLE IF NOT EXISTS requests (
    -- Primary key
    id UUID PRIMARY KEY,

    -- Request details
    endpoint TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    body TEXT NOT NULL,
    api_key TEXT NOT NULL,
    model TEXT NOT NULL,

    -- Status tracking
    status TEXT NOT NULL CHECK (status IN ('pending', 'processing', 'completed', 'failed', 'canceled')),

    -- Retry configuration (per-request)
    max_retries INTEGER NOT NULL DEFAULT 3,
    retry_count INTEGER NOT NULL DEFAULT 0,
    backoff_ms BIGINT NOT NULL DEFAULT 1000,
    timeout_ms BIGINT NOT NULL DEFAULT 30000,

    -- Daemon tracking (for multi-daemon support)
    daemon_id TEXT,
    acquired_at TIMESTAMPTZ,

    -- Next retry scheduling
    next_retry_after TIMESTAMPTZ,

    -- Response data (populated on completion)
    response_status INTEGER,
    response_body TEXT,

    -- Error data (populated on failure)
    error TEXT,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    failed_at TIMESTAMPTZ,
    canceled_at TIMESTAMPTZ
);

-- Index for daemon polling: find pending requests by model, ordered by creation time
CREATE INDEX IF NOT EXISTS idx_requests_pending_by_model
    ON requests(status, model, created_at)
    WHERE status = 'pending';

-- Index for status lookups
CREATE INDEX IF NOT EXISTS idx_requests_status
    ON requests(status);

-- Index for finding stale locks (requests being processed for too long)
CREATE INDEX IF NOT EXISTS idx_requests_stale_locks
    ON requests(acquired_at)
    WHERE status = 'processing';

-- Function to automatically update the updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Trigger to call the function before each update
CREATE TRIGGER update_requests_updated_at
    BEFORE UPDATE ON requests
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
