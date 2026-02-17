-- Migration: Covering index for per-model usage queries scoped to a single user's batched requests.
-- Enables index-only scans for the user usage dashboard's per-model breakdown.
-- Partial index on fusillade_batch_id IS NOT NULL ensures only batched requests are indexed.

CREATE INDEX idx_analytics_user_model_batched ON http_analytics
    (user_id, model) INCLUDE (prompt_tokens, completion_tokens, total_cost, fusillade_batch_id)
    WHERE user_id IS NOT NULL AND model IS NOT NULL AND fusillade_batch_id IS NOT NULL;
