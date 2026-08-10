-- Scope composite components to request classes, and attach the per-route
-- continuation config to the completions component.
--
-- Background: a composite pool's members are interchangeable today — every one
-- of them may serve every request. Mid-stream continuation needs one member per
-- model that is the VALIDATED target for `/v1/completions` token-id resume legs
-- (e.g. Fireworks), and the third-party chat failovers must never receive that
-- traffic (they have never been validated to continue a token-id prefix, and a
-- resume leg landing there produces a plausible-looking wrong answer).
--
-- `role` is what onwards' sync emits as the member's `serves` tag:
--   both        - eligible for every request (on-prem/dynamo members)
--   chat        - everything EXCEPT completions-class requests (third-party
--                 failovers)
--   completions - ONLY completions-class requests (the validated target)
--
-- onwards engages the filter only when a pool actually has a `completions`
-- member, so this migration is behaviour-neutral for every existing composite
-- whatever the backfill picks: with no completions member the pool view is
-- unchanged for both request classes.
ALTER TABLE deployed_model_components
ADD COLUMN role VARCHAR NOT NULL DEFAULT 'chat'
    CHECK (role IN ('both', 'chat', 'completions'));

COMMENT ON COLUMN deployed_model_components.role IS
  'Which request classes this member serves: both | chat | completions. Emitted to onwards as the provider''s `serves` tag. At most one completions member per composite.';

-- Backfill. Two rules, both conservative:
--   1. A member whose deployment is `trusted` is a self-hosted/on-prem upstream
--      (that is what the flag means everywhere else in the sync — it is what
--      gates error-sanitization bypass and trace-context propagation), so it is
--      `both`: dynamo should serve resume legs first, being free for us and
--      failing fast when the model is not live.
--   2. A composite with exactly one component is `both` regardless: it has no
--      failover story today, and leaving its sole member `chat` would silently
--      exclude it the day someone attaches a completions target.
-- Everything else keeps the `chat` default: third-party failovers.
WITH component_counts AS (
    SELECT composite_model_id, COUNT(*) AS n
    FROM deployed_model_components
    GROUP BY composite_model_id
)
UPDATE deployed_model_components dmc
SET role = 'both'
FROM deployed_models dm, component_counts cc
WHERE dm.id = dmc.deployed_model_id
  AND cc.composite_model_id = dmc.composite_model_id
  AND (dm.trusted = TRUE OR cc.n = 1);

-- At most one completions member per composite (v1 simplification: the resume
-- path has no reason to fail over between validated targets yet, and a second
-- one would silently double the "which target served this seam?" question).
-- A partial unique index enforces it without constraining the other roles.
CREATE UNIQUE INDEX idx_deployed_model_components_one_completions
    ON deployed_model_components (composite_model_id)
    WHERE role = 'completions';

-- Per-route continuation config. It lives on the completions component because
-- that is the row describing the validated target: the same row is what makes a
-- model resumable at all, so "is this model resumable" and "how do we render for
-- it" stop being configured in two places.
ALTER TABLE deployed_model_components
ADD COLUMN strip_leading_bos BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN deployed_model_components.strip_leading_bos IS
  'Continuation targets only: drop the leading BOS token from the rendered prefix because this provider prepends its own (Fireworks does on most models). Detected per model during provider onboarding.';

ALTER TABLE deployed_model_components
ADD COLUMN render_kwargs JSONB;

COMMENT ON COLUMN deployed_model_components.render_kwargs IS
  'Continuation targets only: chat_template_kwargs sent to tokenizer-svc /v1/render, and the source of truth for the serving mode the reconstructor must match (e.g. {"thinking_mode": "chat"} for a chat-mode DeepSeek route). NULL = the model''s template default.';

ALTER TABLE deployed_model_components
ADD COLUMN continuation_validated_at TIMESTAMPTZ;

COMMENT ON COLUMN deployed_model_components.continuation_validated_at IS
  'Continuation targets only: when the onboarding harness last certified this route (token-id fidelity, SSE, usage shape). Informational — routing does not gate on it.';
