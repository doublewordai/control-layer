-- Conversation affinity for priority composites: onwards decides
-- preferred-first once per conversation. NULL keeps per-request selection.
-- Fail fast rather than queue routing reads behind a blocked ALTER.
SET LOCAL lock_timeout = '5s';

ALTER TABLE deployed_models
    ADD COLUMN affinity JSONB CHECK (affinity IS NULL OR jsonb_typeof(affinity) = 'object');

-- Protect merged PATCH invariants from concurrent updates to separate fields.
ALTER TABLE deployed_models ADD CONSTRAINT affinity_requires_priority
    CHECK (affinity IS NULL OR COALESCE(affinity->'enabled' = 'false'::JSONB, FALSE) OR (
        is_composite AND lb_strategy = 'priority' AND fallback_enabled IS TRUE
    ) IS TRUE);
