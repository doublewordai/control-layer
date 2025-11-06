-- Complete schema for the fusillade system
-- Tables: files, request_templates, batches, requests

-- Files: Lightweight metadata for grouping templates
CREATE TABLE files (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Request templates: Individual request definitions (mutable)
CREATE TABLE request_templates (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_id UUID NOT NULL REFERENCES files(id) ON DELETE CASCADE,

    -- Request definition
    endpoint TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL,
    api_key TEXT NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Batches: Execution triggers
CREATE TABLE batches (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_id UUID NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Requests: Actual request runs (denormalized snapshot from templates)
CREATE TABLE requests (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    batch_id UUID NOT NULL REFERENCES batches(id) ON DELETE CASCADE,
    template_id UUID NOT NULL REFERENCES request_templates(id) ON DELETE CASCADE,

    -- Denormalized snapshot of template at execution time
    endpoint TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL,
    api_key TEXT NOT NULL,

    -- State machine
    state TEXT NOT NULL DEFAULT 'pending',

    -- State-specific fields (nullable)
    retry_attempt INTEGER NOT NULL DEFAULT 0,
    not_before TIMESTAMPTZ NULL,
    daemon_id UUID NULL,
    claimed_at TIMESTAMPTZ NULL,
    started_at TIMESTAMPTZ NULL,
    response_status SMALLINT NULL,
    response_body TEXT NULL,
    completed_at TIMESTAMPTZ NULL,
    error TEXT NULL,
    failed_at TIMESTAMPTZ NULL,
    canceled_at TIMESTAMPTZ NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- State validation
    CONSTRAINT state_check CHECK (
        state IN ('pending', 'claimed', 'processing', 'completed', 'failed', 'canceled')
    ),

    -- State constraints
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

-- Indexes
CREATE INDEX idx_request_templates_file_id ON request_templates(file_id);
CREATE INDEX idx_batches_file_id ON batches(file_id);
CREATE INDEX idx_batches_created_at ON batches(created_at DESC);
CREATE INDEX idx_requests_batch_id ON requests(batch_id);
CREATE INDEX idx_requests_template_id ON requests(template_id);
CREATE INDEX idx_requests_state ON requests(state);
CREATE INDEX idx_requests_model ON requests(model);
CREATE INDEX idx_requests_daemon ON requests(daemon_id) WHERE daemon_id IS NOT NULL;
CREATE INDEX idx_requests_created_at ON requests(created_at);
CREATE INDEX idx_requests_pending_claim ON requests(state, not_before) WHERE state = 'pending';

-- View: Batch status computed on read
CREATE VIEW batch_status AS
SELECT
    b.id as batch_id,
    b.file_id,
    f.name as file_name,
    COUNT(*) as total_requests,
    COUNT(*) FILTER (WHERE r.state = 'pending') as pending_requests,
    COUNT(*) FILTER (WHERE r.state IN ('claimed', 'processing')) as in_progress_requests,
    COUNT(*) FILTER (WHERE r.state = 'completed') as completed_requests,
    COUNT(*) FILTER (WHERE r.state = 'failed') as failed_requests,
    COUNT(*) FILTER (WHERE r.state = 'canceled') as canceled_requests,
    MIN(r.created_at) as started_at,
    MAX(r.updated_at) as last_updated_at,
    b.created_at
FROM batches b
JOIN files f ON f.id = b.file_id
LEFT JOIN requests r ON r.batch_id = b.id
GROUP BY b.id, b.file_id, f.name, b.created_at;

-- Function to update updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Triggers for auto-updating updated_at
CREATE TRIGGER update_files_updated_at
    BEFORE UPDATE ON files
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER update_request_templates_updated_at
    BEFORE UPDATE ON request_templates
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER update_requests_updated_at
    BEFORE UPDATE ON requests
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Function for LISTEN/NOTIFY on request updates
CREATE OR REPLACE FUNCTION notify_request_update()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        'request_updates',
        json_build_object(
            'id', NEW.id::text,
            'state', NEW.state::text,
            'updated_at', NEW.updated_at::text
        )::text
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Trigger for LISTEN/NOTIFY on request updates
CREATE TRIGGER request_update_notify
    AFTER INSERT OR UPDATE ON requests
    FOR EACH ROW
    EXECUTE FUNCTION notify_request_update();
