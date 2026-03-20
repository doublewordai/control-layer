-- Recompute user_model_usage to include realtime requests.
-- Previously the aggregation only included batch requests (fusillade_batch_id IS NOT NULL).
-- Rebuild the summary table inline to avoid a slow first request after deploy.

TRUNCATE user_model_usage;

INSERT INTO user_model_usage (user_id, model, input_tokens, output_tokens, cost, request_count)
SELECT user_id,
       model,
       COALESCE(SUM(prompt_tokens), 0),
       COALESCE(SUM(completion_tokens), 0),
       COALESCE(SUM(total_cost), 0),
       COUNT(*)
FROM http_analytics
WHERE user_id IS NOT NULL AND model IS NOT NULL
GROUP BY user_id, model
ON CONFLICT (user_id, model)
DO UPDATE SET
    input_tokens = EXCLUDED.input_tokens,
    output_tokens = EXCLUDED.output_tokens,
    cost = EXCLUDED.cost,
    request_count = EXCLUDED.request_count,
    updated_at = NOW();

UPDATE user_model_usage_cursor
SET last_processed_id = COALESCE((SELECT MAX(id) FROM http_analytics), 0),
    updated_at = NOW()
WHERE id = TRUE;
