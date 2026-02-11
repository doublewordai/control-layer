-- Add webhook configuration tables for Standard Webhooks-compliant notifications
-- Supports batch terminal state events (completed, failed, cancelled)

-- Table: user_webhooks (multiple webhooks per user)
CREATE TABLE user_webhooks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    -- Secret prefixed with 'whsec_' followed by base64-encoded 32 bytes
    secret TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT true,
    -- Filter events: null means all events, otherwise array of event types
    event_types JSONB DEFAULT NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Circuit breaker: track consecutive failures
    consecutive_failures INT NOT NULL DEFAULT 0,
    -- When circuit breaker trips, this is set; user must re-enable manually
    disabled_at TIMESTAMPTZ DEFAULT NULL
);

-- Index for listing webhooks by user
CREATE INDEX idx_user_webhooks_user_id ON user_webhooks(user_id);

-- Index for finding enabled webhooks
CREATE INDEX idx_user_webhooks_enabled ON user_webhooks(user_id, enabled) WHERE enabled = true;

-- Table: webhook_deliveries (delivery tracking and retry management)
CREATE TABLE webhook_deliveries (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    webhook_id UUID NOT NULL REFERENCES user_webhooks(id) ON DELETE CASCADE,
    -- The webhook-id header value (unique per delivery attempt series)
    event_id UUID NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    -- pending, delivered, failed, exhausted
    status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INT NOT NULL DEFAULT 0,
    -- For retry scheduling
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Reference to the batch that triggered this event
    batch_id UUID NOT NULL,
    -- Last HTTP status code received (null if no response yet)
    last_status_code INT,
    -- Last error message (for debugging)
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Index for finding pending deliveries to retry
CREATE INDEX idx_webhook_deliveries_pending ON webhook_deliveries(next_attempt_at)
    WHERE status IN ('pending', 'failed');

-- Index for finding deliveries by webhook
CREATE INDEX idx_webhook_deliveries_webhook_id ON webhook_deliveries(webhook_id);

-- Index for finding deliveries by batch
CREATE INDEX idx_webhook_deliveries_batch_id ON webhook_deliveries(batch_id);

-- Updated_at trigger for user_webhooks
CREATE TRIGGER user_webhooks_updated_at
    BEFORE UPDATE ON user_webhooks
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Updated_at trigger for webhook_deliveries
CREATE TRIGGER webhook_deliveries_updated_at
    BEFORE UPDATE ON webhook_deliveries
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Note: Webhook delivery uses polling to detect batch terminal states
-- rather than database triggers, to avoid cross-schema dependencies on fusillade.
