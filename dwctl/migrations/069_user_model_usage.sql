-- Migration: Pre-aggregated per-user per-model usage summary.
-- Converts O(N) scan of http_analytics into O(M) lookup where M = distinct models per user.
-- Uses incremental aggregation with a cursor to process only new rows on each refresh.

CREATE TABLE user_model_usage (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cost DECIMAL(24, 15) NOT NULL DEFAULT 0,
    request_count BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, model)
);

-- Single-row cursor table tracking the last processed http_analytics.id.
CREATE TABLE user_model_usage_cursor (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id = TRUE),
    last_processed_id BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed cursor so incremental refresh works immediately.
-- The first all-time usage request will backfill from id 0.
INSERT INTO user_model_usage_cursor (last_processed_id) VALUES (0);
