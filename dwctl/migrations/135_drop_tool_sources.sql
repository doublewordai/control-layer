-- Finish the #878 "server-side tool calling" teardown.
--
-- The server-side tool loop was removed first (COR-517 / COR-536). Without it the
-- remaining half of the feature - admin-registered tool_sources injected into the
-- model's `tools` array - could only produce tool_calls the caller cannot fulfil,
-- because the tool URL and auth live server-side. The injection middleware and the
-- /tool-sources admin API are removed in the same change as this migration, so the
-- tables below have no remaining reader or writer.
--
-- Dropped:
--   * tool_sources, deployment_tool_sources, group_tool_sources (migration 082)
--   * tool_call_analytics (migration 082) - per-call detail rows for server-side
--     tool executions; FK on tool_sources, and its only writer was the executor.
--   * http_analytics.tool_iterations (migration 082) - server-side loop step count.
--   * http_analytics.response_step_id and tool_call_analytics.response_step_id
--     (migration 096) - correlation to the retired multi-step loop's step rows.
--   * deployed_models.open_responses_adapter (migration 068) - toggled the onwards
--     Responses adapter, which is removed alongside this: Responses requests are
--     always translated at the dwctl edge before they reach onwards, so the flag
--     has no reader and its admin API field goes with it.
--
-- Client-side tool use is unaffected: it is observed via http_analytics.finish_reason
-- (migration 125), which never depended on any of the above.

DROP TABLE IF EXISTS tool_call_analytics;
DROP TABLE IF EXISTS deployment_tool_sources;
DROP TABLE IF EXISTS group_tool_sources;
DROP TABLE IF EXISTS tool_sources;

DROP INDEX IF EXISTS idx_analytics_response_step_id;
ALTER TABLE http_analytics DROP COLUMN IF EXISTS response_step_id;
ALTER TABLE http_analytics DROP COLUMN IF EXISTS tool_iterations;

ALTER TABLE deployed_models DROP COLUMN IF EXISTS open_responses_adapter;
