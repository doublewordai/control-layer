-- Logged, durable producer queue. No bodies, raw SSE, credentials or JSON payload.
CREATE TABLE fusillade_billing_events (
    event_id UUID PRIMARY KEY,
    request_id UUID NOT NULL,
    retry_attempt BIGINT NOT NULL CHECK (retry_attempt >= 0),
    owner_id UUID NOT NULL,
    -- Snapshot attribution while the key still exists. No secret is persisted.
    -- NULL means unresolved and requires reconciliation before consumption.
    api_key_id UUID,
    api_key_purpose TEXT,
    cap_scope_root UUID,
    batch_id UUID,
    requested_model TEXT NOT NULL,
    response_model TEXT,
    upstream_response_id TEXT,
    completion_window TEXT,
    batch_created_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    usage_present BOOLEAN NOT NULL,
    prompt_tokens BIGINT CHECK (prompt_tokens >= 0),
    completion_tokens BIGINT CHECK (completion_tokens >= 0),
    total_tokens BIGINT CHECK (total_tokens >= 0),
    reasoning_tokens BIGINT CHECK (reasoning_tokens >= 0),
    engine_cached_tokens BIGINT CHECK (engine_cached_tokens >= 0),
    cache_read_input_tokens BIGINT CHECK (cache_read_input_tokens >= 0),
    cache_creation_5m_input_tokens BIGINT CHECK (cache_creation_5m_input_tokens >= 0),
    cache_creation_1h_input_tokens BIGINT CHECK (cache_creation_1h_input_tokens >= 0),
    cache_creation_24h_input_tokens BIGINT CHECK (cache_creation_24h_input_tokens >= 0),
    -- Shadow events retain legacy charging and never authorize a second debit.
    captured_under_legacy_billing BOOLEAN NOT NULL DEFAULT TRUE,
    processed_at TIMESTAMPTZ
);

CREATE INDEX fusillade_billing_events_pending_idx
    ON fusillade_billing_events (created_at, event_id) WHERE processed_at IS NULL;
COMMENT ON INDEX fusillade_billing_events_pending_idx IS
    'Pending capture index; unresolved events must not expire.';
COMMENT ON TABLE fusillade_billing_events IS
    'Scalar usage captured from existing Fusillade stream reassembly. Legacy captures are shadow-only; durable captures require accepted-result evidence. Expire only processed events.';

ALTER TABLE fusillade_billing_events
    ADD COLUMN billing_mode TEXT NOT NULL DEFAULT 'legacy' CHECK (billing_mode IN ('legacy', 'durable')),
    ADD COLUMN event_version SMALLINT NOT NULL DEFAULT 1,
    ADD COLUMN model_id UUID,
    ADD COLUMN request_path TEXT NOT NULL DEFAULT '/v1/chat/completions',
    ADD COLUMN request_method TEXT NOT NULL DEFAULT 'POST',
    ADD COLUMN custom_id TEXT,
    ADD COLUMN finish_reason TEXT,
    ADD COLUMN served_by TEXT,
    ADD COLUMN processing_state TEXT NOT NULL DEFAULT 'pending' CHECK (processing_state IN ('pending', 'processed', 'unresolved')),
    ADD COLUMN attempt_count BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN error_code TEXT,
    ADD COLUMN disposition TEXT;

CREATE INDEX fusillade_billing_events_due_idx
    ON fusillade_billing_events (next_attempt_at, event_id)
    WHERE processing_state = 'pending';
CREATE INDEX fusillade_billing_events_retention_idx
    ON fusillade_billing_events (processed_at, event_id) WHERE processed_at IS NOT NULL;
CREATE INDEX fusillade_billing_events_request_idx ON fusillade_billing_events (request_id);

-- Kept independently of queue payload retention. No FK to ephemeral events or analytics.
CREATE TABLE billing_receipts (
    request_id UUID PRIMARY KEY,
    owner_id UUID NOT NULL,
    event_id UUID NOT NULL,
    total_cost NUMERIC(30, 15) NOT NULL CHECK (total_cost >= 0),
    ledger_source_id TEXT NOT NULL UNIQUE,
    analytics_id BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE billing_worker_heartbeat (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    last_seen_at TIMESTAMPTZ NOT NULL
);
COMMENT ON TABLE billing_receipts IS
    'Durable billing deduplication. Retain beyond queue cleanup and for the entire permitted replay horizon.';
ALTER TABLE billing_receipts
    ADD COLUMN input_price_per_token NUMERIC(30,15),
    ADD COLUMN output_price_per_token NUMERIC(30,15),
    ADD COLUMN uncached_cost NUMERIC(30,15);

ALTER TABLE fusillade_billing_events ADD COLUMN lease_owner UUID, ADD COLUMN lease_until TIMESTAMPTZ;
