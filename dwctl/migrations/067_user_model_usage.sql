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

-- Backfill from existing data
INSERT INTO user_model_usage (user_id, model, input_tokens, output_tokens, cost, request_count)
SELECT user_id,
       model,
       COALESCE(SUM(prompt_tokens), 0),
       COALESCE(SUM(completion_tokens), 0),
       COALESCE(SUM(total_cost), 0),
       COUNT(*)
FROM http_analytics
WHERE user_id IS NOT NULL AND model IS NOT NULL AND fusillade_batch_id IS NOT NULL
GROUP BY user_id, model;

-- Set cursor to current max id
INSERT INTO user_model_usage_cursor (last_processed_id)
VALUES (COALESCE((SELECT MAX(id) FROM http_analytics), 0));
