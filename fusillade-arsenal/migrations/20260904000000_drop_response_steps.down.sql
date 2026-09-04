-- Recreates the table at its final shape (20260428 + 20260430). Rows are not
-- restored: the writer no longer exists.
CREATE TABLE IF NOT EXISTS response_steps (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    request_id      UUID NULL REFERENCES requests(id) ON DELETE CASCADE,
    prev_step_id    UUID NULL REFERENCES response_steps(id) ON DELETE CASCADE,
    parent_step_id  UUID NULL REFERENCES response_steps(id) ON DELETE CASCADE,
    step_kind       TEXT NOT NULL,
    step_sequence   BIGINT NOT NULL,
    request_payload JSONB NOT NULL,
    response_payload JSONB NULL,
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
        CHECK (state IN ('pending', 'processing', 'completed', 'failed', 'canceled')),
    CONSTRAINT response_steps_request_id_step_kind_check CHECK (
        (step_kind = 'model_call' AND request_id IS NOT NULL) OR
        (step_kind = 'tool_call'  AND request_id IS NULL)
    )
);
CREATE UNIQUE INDEX IF NOT EXISTS response_steps_request_id_unique
    ON response_steps (request_id) WHERE request_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS response_steps_chain_walk
    ON response_steps (parent_step_id, step_sequence) WHERE parent_step_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS response_steps_prev
    ON response_steps (prev_step_id) WHERE prev_step_id IS NOT NULL;
