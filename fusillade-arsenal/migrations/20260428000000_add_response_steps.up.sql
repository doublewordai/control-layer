-- Add response_steps table for multi-step Open Responses orchestration.
--
-- Each row is a discrete unit of work in a multi-step response: either a
-- model_call (upstream LLM invocation) or a tool_call (server-side tool
-- execution). Rows are linearly chained per (request_id, parent_step_id)
-- via prev_step_id, with parent_step_id pointing at the enclosing
-- tool_call step for nested sub-agent loops (NULL for top-level steps).
--
-- See fusillade/docs/plans/2026-04-28-multi-step-responses.md.
--
-- Pre-deploy: a fresh table — no large-table locking concerns. CREATE
-- INDEX CONCURRENTLY is not required.

CREATE TABLE IF NOT EXISTS response_steps (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    request_id      UUID NOT NULL REFERENCES requests(id) ON DELETE CASCADE,

    -- Linear predecessor within a single chain (NULL = first step in its
    -- scope). Scope is (request_id, parent_step_id): the chain restarts
    -- inside each nested sub-loop.
    --
    -- ON DELETE CASCADE so a parent-row delete (or a manual single-row
    -- delete in the chain) does not leave dangling pointers. The cascade
    -- from `requests` already deletes every step in one statement, but
    -- being explicit here keeps individual-step deletes safe too.
    prev_step_id    UUID NULL REFERENCES response_steps(id) ON DELETE CASCADE,

    -- Nesting pointer for sub-agent loops (NULL = top-level step in the
    -- user-visible response). ON DELETE CASCADE for the same reason as
    -- prev_step_id.
    parent_step_id  UUID NULL REFERENCES response_steps(id) ON DELETE CASCADE,

    step_kind       TEXT NOT NULL,

    -- Monotonic per request_id, global across nesting levels. Doubles as
    -- the Last-Event-ID stream cursor for top-level events.
    step_sequence   BIGINT NOT NULL,

    -- Step inputs (instructions only; full HTTP body is assembled at
    -- fire time from the chain walk, not denormalized into each row).
    request_payload JSONB NOT NULL,

    -- Step output (NULL until the step reaches a terminal state).
    response_payload JSONB NULL,

    -- State machine — mirrors fusillade.requests state values. No claim
    -- columns: serialized access is provided by the parent requests row's
    -- lease (held by onwards inline or by a fusillade daemon worker).
    state           TEXT NOT NULL DEFAULT 'pending',

    started_at      TIMESTAMPTZ NULL,
    completed_at    TIMESTAMPTZ NULL,
    failed_at       TIMESTAMPTZ NULL,
    canceled_at     TIMESTAMPTZ NULL,
    retry_attempt   INT NOT NULL DEFAULT 0,
    error           JSONB NULL,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT response_steps_kind_check
        CHECK (step_kind IN ('model_call', 'tool_call')),

    CONSTRAINT response_steps_state_check
        CHECK (state IN ('pending', 'processing',
                         'completed', 'failed', 'canceled')),

    -- State field-presence invariants, mirroring the `requests` table.
    -- These backstop partial-update bugs that would otherwise leave a row
    -- in an inconsistent shape (e.g., `completed` with no
    -- `response_payload`).
    CONSTRAINT response_steps_pending_fields_check CHECK (
        state <> 'pending' OR (
            started_at IS NULL
            AND completed_at IS NULL
            AND failed_at IS NULL
            AND canceled_at IS NULL
            AND response_payload IS NULL
            AND error IS NULL
        )
    ),
    CONSTRAINT response_steps_processing_fields_check CHECK (
        state <> 'processing' OR (
            started_at IS NOT NULL
            AND completed_at IS NULL
            AND failed_at IS NULL
            AND canceled_at IS NULL
            AND response_payload IS NULL
            AND error IS NULL
        )
    ),
    CONSTRAINT response_steps_completed_fields_check CHECK (
        state <> 'completed' OR (
            started_at IS NOT NULL
            AND completed_at IS NOT NULL
            AND failed_at IS NULL
            AND canceled_at IS NULL
            AND response_payload IS NOT NULL
            AND error IS NULL
        )
    ),
    -- `failed` and `canceled` are reachable from either `pending` or
    -- `processing`, so `started_at` may be NULL on these rows. We do not
    -- require it.
    CONSTRAINT response_steps_failed_fields_check CHECK (
        state <> 'failed' OR (
            completed_at IS NULL
            AND failed_at IS NOT NULL
            AND canceled_at IS NULL
            AND response_payload IS NULL
            AND error IS NOT NULL
        )
    ),
    CONSTRAINT response_steps_canceled_fields_check CHECK (
        state <> 'canceled' OR (
            completed_at IS NULL
            AND failed_at IS NULL
            AND canceled_at IS NOT NULL
            AND response_payload IS NULL
        )
    ),

    -- Idempotency safety net for crash recovery: a re-running transition
    -- function must not produce duplicate successor rows. Recovery
    -- primarily relies on a chain walk that identifies the existing
    -- frontier; this constraint backstops any race.
    --
    -- NULLS NOT DISTINCT (PG15+) is required because both
    -- `parent_step_id` (NULL = top-level) and `prev_step_id` (NULL =
    -- first in scope) are nullable. With the default NULLS DISTINCT
    -- behavior, two `(request_id, NULL, NULL, 'model_call')` rows
    -- would not conflict and the safety net would be void.
    CONSTRAINT response_steps_chain_unique
        UNIQUE NULLS NOT DISTINCT (request_id, parent_step_id, prev_step_id, step_kind)
);

-- Chain walk for a given scope (top-level or sub-loop): the
-- (request_id, parent_step_id) pair identifies the chain, step_sequence
-- orders it. Postgres treats NULL parent_step_id as its own group.
CREATE INDEX IF NOT EXISTS response_steps_chain
    ON response_steps (request_id, parent_step_id, step_sequence);

-- Predecessor lookup: find a step's successor (or detect leaf during
-- crash recovery).
CREATE INDEX IF NOT EXISTS response_steps_prev
    ON response_steps (prev_step_id)
    WHERE prev_step_id IS NOT NULL;

-- Sub-loop traversal: gather all sub-steps of a given parent step
-- (e.g., when rendering the dashboard's expandable sub-agent tree).
CREATE INDEX IF NOT EXISTS response_steps_parent
    ON response_steps (parent_step_id)
    WHERE parent_step_id IS NOT NULL;
