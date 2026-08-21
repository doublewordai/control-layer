-- Group a composite's members into NAMED POOLS, and attach the per-route
-- continuation config to the members of the completions pool.
--
-- Background: a composite's members are one interchangeable list today — every
-- one of them may serve every request. Mid-stream continuation needs a separate
-- failover list for `/v1/completions` token-id resume legs: the validated
-- target (e.g. Fireworks) must serve them, and the third-party chat failovers
-- must never receive them (none has been validated to continue a token-id
-- prefix, and a resume leg landing there produces a plausible-looking wrong
-- answer).
--
-- The structure that expresses this is a pool per request class, not a tag per
-- member: onwards resolves `request class -> pool` once, before selection, and
-- the load-balance strategy, failover loop and limiters then run on the chosen
-- pool unchanged. "Never serves chat" becomes structural — the validated target
-- is simply not a member of the default pool.
ALTER TABLE deployed_model_components
ADD COLUMN pool VARCHAR NOT NULL DEFAULT 'default'
    CHECK (pool IN ('default', 'completions'));

COMMENT ON COLUMN deployed_model_components.pool IS
  'Which of the composite''s named pools this membership belongs to: default | completions. `default` serves every request class without a pool of its own, and is exactly the single pool that existed before pools. Emitted to onwards as the pool name.';

-- Membership is per pool, so the same hosted model can be a member of more than
-- one — dynamo sits at position 0 of BOTH the default and the completions pool
-- (free for us, fails fast when the model is not live), with a different
-- failover behind it in each.
ALTER TABLE deployed_model_components
DROP CONSTRAINT deployed_model_components_composite_model_id_deployed_model_key;

ALTER TABLE deployed_model_components
ADD CONSTRAINT deployed_model_components_composite_model_pool_key
    UNIQUE (composite_model_id, deployed_model_id, pool);

-- Ordering is per pool too: each pool is independently drag-ordered, so
-- `sort_order` is dense 0..n-1 within a (composite, pool), not within a
-- composite.
DROP INDEX idx_deployed_model_components_sort_order;

CREATE INDEX idx_deployed_model_components_sort_order
    ON deployed_model_components (composite_model_id, pool, sort_order);

-- No backfill: every existing membership defaults to `default`, which is
-- byte-identically the single pool it was in before. A composite gains a
-- completions pool only when somebody attaches a member to it.

-- Per-route continuation config. It lives on the completions pool's members
-- because those are the rows describing a validated target: the same rows are
-- what make a model resumable at all, so "is this model resumable" and "how do
-- we render for it" stop being configured in two places.
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
