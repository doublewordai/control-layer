-- Durable hand-off between response capture and analytics/billing projection.
-- payload contains a content-free RawAnalyticsRecord. It identifies an
-- authenticated request by api_key_id; API-key bearer credentials are never
-- stored in this table.
CREATE TABLE analytics_outbox (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    instance_id UUID NOT NULL,
    correlation_id BIGINT NOT NULL,
    payload JSONB NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (instance_id, correlation_id)
);

CREATE INDEX analytics_outbox_created_at_idx
    ON analytics_outbox (created_at);
