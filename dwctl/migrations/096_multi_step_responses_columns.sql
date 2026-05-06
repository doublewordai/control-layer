-- Multi-step Open Responses orchestration: dwctl analytics linkage and
-- tool_sources extension.
--
-- See fusillade/docs/plans/2026-04-28-multi-step-responses.md.
--
-- Two changes:
--
-- 1. Add `response_step_id` to http_analytics and tool_call_analytics so
--    every upstream HTTP call (model fire or tool fire) recorded by the
--    outlet middleware can be correlated to the response_step row that
--    drove it. Mirrors the existing `fusillade_request_id` /
--    `fusillade_batch_id` correlation pattern.
--
-- 2. Add `kind` semantics to tool_sources for sub-agent dispatch.
--    The column already exists with default 'http' (per migration 082);
--    this widens the implicit set of valid values to include 'agent'
--    (a sub-agent dispatch tool — exercises run_response_loop's
--    recursion path) by adding a CHECK constraint that documents both
--    values explicitly.

ALTER TABLE http_analytics
    ADD COLUMN IF NOT EXISTS response_step_id UUID NULL;

CREATE INDEX IF NOT EXISTS idx_analytics_response_step_id
    ON http_analytics (response_step_id)
    WHERE response_step_id IS NOT NULL;

ALTER TABLE tool_call_analytics
    ADD COLUMN IF NOT EXISTS response_step_id UUID NULL;

CREATE INDEX IF NOT EXISTS idx_tool_call_analytics_response_step_id
    ON tool_call_analytics (response_step_id)
    WHERE response_step_id IS NOT NULL;

COMMENT ON COLUMN http_analytics.response_step_id IS
    'Optional FK to fusillade.response_steps for multi-step responses. NULL for non-multi-step requests.';

COMMENT ON COLUMN tool_call_analytics.response_step_id IS
    'Optional FK to fusillade.response_steps. Set when the tool call originated from a response_steps row rather than the legacy in-process tool loop.';

-- Document and constrain the tool_sources.kind set. NOT VALID skips the
-- full-table validation scan; existing rows are guaranteed to satisfy
-- the constraint because 'http' is the only value used pre-migration.
DO $$ BEGIN
  ALTER TABLE tool_sources ADD CONSTRAINT tool_sources_kind_check
    CHECK (kind IN ('http', 'agent')) NOT VALID;
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

COMMENT ON COLUMN tool_sources.kind IS
    'Tool dispatch mechanism. ''http'' = standard HTTP tool (default). ''agent'' = sub-agent dispatch (run_response_loop recursion path). Future kinds may include ''mcp''.';
