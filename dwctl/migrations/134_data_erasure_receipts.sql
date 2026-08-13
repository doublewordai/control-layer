-- Durable request/receipt for creator-scoped erasure. The raw subject exists
-- only while work is pending; completion clears it and retains a one-way
-- fingerprint plus timestamps as operational evidence.
CREATE TABLE data_erasure_requests (
    id UUID PRIMARY KEY,
    subject_id TEXT,
    subject_fingerprint BYTEA NOT NULL CHECK (OCTET_LENGTH(subject_fingerprint) = 32),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'completed')),
    capture_store_applicable BOOLEAN NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    last_attempt_at TIMESTAMPTZ,
    request_store_completed_at TIMESTAMPTZ,
    capture_store_completed_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    failure_target TEXT CHECK (failure_target IN ('request_store', 'capture_store')),
    CONSTRAINT data_erasure_subject_lifecycle CHECK (
        (status = 'completed' AND subject_id IS NULL AND completed_at IS NOT NULL)
        OR (status <> 'completed' AND subject_id IS NOT NULL AND completed_at IS NULL)
    ),
    CONSTRAINT data_erasure_capture_completion CHECK (
        capture_store_applicable OR capture_store_completed_at IS NULL
    ),
    CONSTRAINT data_erasure_completed_targets CHECK (
        status <> 'completed'
        OR (
            request_store_completed_at IS NOT NULL
            AND (NOT capture_store_applicable OR capture_store_completed_at IS NOT NULL)
        )
    ),
    UNIQUE (subject_fingerprint)
);

CREATE INDEX data_erasure_requests_pending_idx
    ON data_erasure_requests (requested_at, id)
    WHERE status <> 'completed';

CREATE INDEX data_erasure_requests_completed_idx
    ON data_erasure_requests (completed_at, id)
    WHERE status = 'completed';

COMMENT ON TABLE data_erasure_requests IS
    'Durable creator erasure outbox and one-way completion evidence.';
