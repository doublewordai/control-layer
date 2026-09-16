-- Default-off load-aware routing and per-model streaming deadline.
ALTER TABLE deployed_models
    ADD COLUMN first_token_timeout_ms BIGINT CHECK (first_token_timeout_ms BETWEEN 0 AND 3600000),
    ADD COLUMN aimd JSONB CHECK (aimd IS NULL OR jsonb_typeof(aimd) = 'object');

-- Protect merged PATCH invariants from concurrent updates to separate fields.
ALTER TABLE deployed_models ADD CONSTRAINT aimd_requires_priority_and_deadline
    CHECK (aimd IS NULL OR (
        is_composite AND lb_strategy = 'priority' AND fallback_enabled IS TRUE
        AND first_token_timeout_ms IS NOT NULL
        AND (first_token_timeout_ms = 0 OR first_token_timeout_ms >= (aimd->>'latency_budget_ms')::BIGINT)
    ) IS TRUE);
