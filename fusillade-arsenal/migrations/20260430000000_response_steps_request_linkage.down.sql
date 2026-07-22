-- Reverse the response_steps re-anchoring. Restores the schema to its
-- 20260428000000_add_response_steps shape.
--
-- Pre-deploy: this rollback assumes any tool_call rows inserted under
-- the new schema (with request_id NULL) have been hand-resolved before
-- the migration runs. The NOT NULL re-add will fail otherwise.

DROP INDEX IF EXISTS response_steps_chain_walk;
DROP INDEX IF EXISTS response_steps_request_id_unique;

CREATE INDEX IF NOT EXISTS response_steps_parent
    ON response_steps (parent_step_id)
    WHERE parent_step_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS response_steps_chain
    ON response_steps (request_id, parent_step_id, step_sequence);

ALTER TABLE response_steps
    ADD CONSTRAINT response_steps_chain_unique
    UNIQUE NULLS NOT DISTINCT (request_id, parent_step_id, prev_step_id, step_kind);

ALTER TABLE response_steps
    DROP CONSTRAINT response_steps_request_id_step_kind_check;

ALTER TABLE response_steps
    ALTER COLUMN request_id SET NOT NULL;
