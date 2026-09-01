-- Re-anchor response_steps.request_id at the per-step sub-request
-- fusillade row (the row created for that step's HTTP fire) instead
-- of the parent /v1/responses request. parent_step_id becomes the
-- chain identifier ("root_step_id"): NULL only on the head, the
-- head's id on every other step in the chain.
--
-- See fusillade/docs/plans/2026-04-30-response-steps-request-linkage.md
-- for the full design and rationale (including why the chain
-- idempotency unique constraint is dropped rather than re-keyed —
-- branching is intrinsic to the data model).

-- 1. request_id becomes nullable. tool_call steps don't have a
-- corresponding fusillade.requests row (tool dispatch goes through
-- dwctl's HttpToolExecutor; analytics live in tool_call_analytics).
-- Only model_call steps have a sub-request row.
ALTER TABLE response_steps
    ALTER COLUMN request_id DROP NOT NULL;

-- 1a. Enforce the per-kind nullability invariant at the DB level so
-- get_step_by_request and analytics attribution can rely on it
-- without runtime defensiveness:
--   model_call ⇒ request_id IS NOT NULL  (the sub-request row this
--                step's HTTP fire created — 1:1 with http_analytics)
--   tool_call  ⇒ request_id IS NULL      (tool dispatch lives outside
--                fusillade.requests; analytics live in
--                tool_call_analytics keyed on response_step_id)
ALTER TABLE response_steps
    ADD CONSTRAINT response_steps_request_id_step_kind_check
    CHECK (
        (step_kind = 'model_call' AND request_id IS NOT NULL) OR
        (step_kind = 'tool_call'  AND request_id IS NULL)
    );

-- 2. Drop the chain idempotency constraint.
--
-- Branching is intrinsic to the data model: a model_call returning
-- multiple tool_calls (parallel tool dispatch — standard OpenAI shape)
-- emits N tool_call children sharing
-- (parent_step_id, model_call_id, 'tool_call'). Any (parent, prev,
-- kind) uniqueness rejects this, regardless of sub-agent nesting.
--
-- Idempotency falls to the transition function's chain-walk frontier
-- check in next_action_for: walk the existing chain, emit only the
-- successors that aren't already there. A redundant duplicate row is
-- observable (extra entries in the chain walk) and recoverable, not
-- corrupting.
ALTER TABLE response_steps
    DROP CONSTRAINT response_steps_chain_unique;

-- 3. 1:1 between a model_call step and its sub-request fusillade row.
-- Doubles as the FK-supporting index for request_id lookups, replacing
-- the role response_steps_chain used to play.
CREATE UNIQUE INDEX response_steps_request_id_unique
    ON response_steps (request_id)
    WHERE request_id IS NOT NULL;

-- 4. Drop the old chain index (anchored on the old request_id
-- semantic where request_id was constant per chain).
DROP INDEX IF EXISTS response_steps_chain;

-- 5. New chain walk index. From a head step's id, range-scan all
-- non-head steps that belong to that response in step_sequence
-- order. Used by GET /v1/responses/{id} (full-tree assembly) and by
-- next_action_for (frontier discovery during crash-recovery resume).
--
-- Partial (parent_step_id IS NOT NULL) keeps the index tight:
-- - The head step is fetched separately by id (PK lookup), so it
--   doesn't need this index;
-- - Skipping head rows halves the index size for the typical 2-step
--   response and shrinks index depth at the high end.
CREATE INDEX response_steps_chain_walk
    ON response_steps (parent_step_id, step_sequence)
    WHERE parent_step_id IS NOT NULL;

-- 6. Drop the simple parent_step_id index — subsumed by
-- response_steps_chain_walk above (same partial predicate, additional
-- ordering key).
DROP INDEX IF EXISTS response_steps_parent;

-- 7. Update the column comments to reflect the new semantics. Index
-- comments already follow the migration's ordering.
COMMENT ON COLUMN response_steps.request_id IS
    'FK to fusillade.requests for the upstream HTTP fire that this step represents. NULL on tool_call steps (tool dispatch lives outside fusillade.requests; analytics live in tool_call_analytics). Unique among non-NULL values via response_steps_request_id_unique.';

COMMENT ON COLUMN response_steps.parent_step_id IS
    'Head step (root) of the user-visible response chain. NULL only on the head itself. Every non-head step in the chain — including parallel tool_call siblings and any sub-agent steps reached via prev_step_id branching — points at the same head id.';

COMMENT ON COLUMN response_steps.prev_step_id IS
    'Tree edge: the step that immediately precedes this one. Multiple steps may share a prev_step_id (parallel tool_calls; sub-agent head + outer continuation after a tool_call). Order within siblings is given by step_sequence.';
