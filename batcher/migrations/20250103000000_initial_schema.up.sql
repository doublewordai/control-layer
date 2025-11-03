-- Initial schema for the batcher system
-- This schema supports the typestate request lifecycle with proper atomicity guarantees

-- Create enum type for request states
CREATE TYPE request_state AS ENUM (
    'pending',
    'claimed',
    'processing',
    'completed',
    'failed',
    'canceled'
);

-- Main requests table with normalized columns
CREATE TABLE requests (
    -- Request identification
    id UUID PRIMARY KEY,

    -- Current state
    state request_state NOT NULL DEFAULT 'pending',

    -- RequestData fields (always present)
    endpoint TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    body TEXT NOT NULL,
    model TEXT NOT NULL,
    api_key TEXT NOT NULL,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Pending state fields
    retry_attempt INTEGER NOT NULL DEFAULT 0,
    not_before TIMESTAMPTZ NULL,

    -- Claimed/Processing state fields
    daemon_id UUID NULL,
    claimed_at TIMESTAMPTZ NULL,

    -- Processing state fields
    started_at TIMESTAMPTZ NULL,

    -- Completed state fields
    response_status SMALLINT NULL,
    response_body TEXT NULL,

    -- Completed/Failed state fields
    completed_at TIMESTAMPTZ NULL,

    -- Failed state fields
    error TEXT NULL,
    failed_at TIMESTAMPTZ NULL,

    -- Canceled state fields
    canceled_at TIMESTAMPTZ NULL,

    -- Constraints to ensure state-specific fields are populated correctly
    CONSTRAINT pending_fields_check CHECK (
        (state != 'pending') OR (daemon_id IS NULL AND claimed_at IS NULL AND started_at IS NULL)
    ),
    CONSTRAINT claimed_fields_check CHECK (
        (state != 'claimed') OR (daemon_id IS NOT NULL AND claimed_at IS NOT NULL AND started_at IS NULL)
    ),
    CONSTRAINT processing_fields_check CHECK (
        (state != 'processing') OR (daemon_id IS NOT NULL AND claimed_at IS NOT NULL AND started_at IS NOT NULL)
    ),
    CONSTRAINT completed_fields_check CHECK (
        (state != 'completed') OR (response_status IS NOT NULL AND response_body IS NOT NULL AND completed_at IS NOT NULL)
    ),
    CONSTRAINT failed_fields_check CHECK (
        (state != 'failed') OR (error IS NOT NULL AND failed_at IS NOT NULL)
    ),
    CONSTRAINT canceled_fields_check CHECK (
        (state != 'canceled') OR (canceled_at IS NOT NULL)
    )
);

-- Indexes for efficient querying

-- Index for claiming pending requests (most critical operation)
-- This supports: WHERE state = 'pending' AND (not_before IS NULL OR not_before <= NOW())
CREATE INDEX idx_requests_pending_claim ON requests (state, not_before)
WHERE state = 'pending';

-- Index for filtering by model (used for concurrency control)
CREATE INDEX idx_requests_model ON requests (model);

-- Index for filtering by daemon_id (for monitoring and unclaim operations)
CREATE INDEX idx_requests_daemon ON requests (daemon_id)
WHERE daemon_id IS NOT NULL;

-- Index for filtering by state
CREATE INDEX idx_requests_state ON requests (state);

-- Index for time-based queries and cleanup
CREATE INDEX idx_requests_created_at ON requests (created_at);

-- Trigger to update the updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ language 'plpgsql';

CREATE TRIGGER update_requests_updated_at
    BEFORE UPDATE ON requests
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Function and trigger for LISTEN/NOTIFY on request updates
-- This enables real-time status updates
CREATE OR REPLACE FUNCTION notify_request_update()
RETURNS TRIGGER AS $$
DECLARE
    payload JSON;
BEGIN
    -- Build a JSON payload with the request ID and new state
    payload = json_build_object(
        'id', NEW.id,
        'state', NEW.state,
        'updated_at', NEW.updated_at
    );

    -- Notify on two channels:
    -- 1. General channel for all updates
    PERFORM pg_notify('request_updates', payload::text);

    -- 2. Request-specific channel for targeted listening
    PERFORM pg_notify('request_update_' || NEW.id::text, payload::text);

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER request_update_notify
    AFTER INSERT OR UPDATE ON requests
    FOR EACH ROW
    EXECUTE FUNCTION notify_request_update();
