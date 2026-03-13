-- Tool sources: HTTP endpoints that can be called as tools during agent loops.
-- Currently only 'http' kind is supported; MCP columns are reserved for future use.

CREATE TABLE tool_sources (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind           TEXT NOT NULL DEFAULT 'http',
    name           TEXT NOT NULL UNIQUE,
    description    TEXT,
    parameters     JSONB,
    url            TEXT NOT NULL,
    api_key        TEXT,
    timeout_secs   INT NOT NULL DEFAULT 30 CHECK (timeout_secs > 0),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Junction table: which tool sources are attached to a deployment.
-- Follows the same pattern as deployment_groups.
CREATE TABLE deployment_tool_sources (
    deployment_id   UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    tool_source_id  UUID NOT NULL REFERENCES tool_sources(id) ON DELETE CASCADE,
    PRIMARY KEY (deployment_id, tool_source_id)
);

-- Junction table: which tool sources are attached to a group.
CREATE TABLE group_tool_sources (
    group_id        UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    tool_source_id  UUID NOT NULL REFERENCES tool_sources(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, tool_source_id)
);

-- Per-call analytics for tool executions.
CREATE TABLE tool_call_analytics (
    id                BIGSERIAL PRIMARY KEY,
    analytics_id      BIGINT REFERENCES http_analytics(id),
    tool_source_id    UUID REFERENCES tool_sources(id),
    tool_name         TEXT NOT NULL,
    started_at        TIMESTAMPTZ NOT NULL,
    duration_ms       BIGINT NOT NULL,
    http_status_code  INT,
    success           BOOL NOT NULL,
    error_kind        TEXT,
    price_per_call    DECIMAL,
    billed_at         TIMESTAMPTZ
);

-- Add tool_iterations to http_analytics for tracking how many tool call rounds occurred.
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS tool_iterations INT;

-- Indexes
CREATE INDEX idx_deployment_tool_sources_deployment_id ON deployment_tool_sources(deployment_id);
CREATE INDEX idx_deployment_tool_sources_tool_source_id ON deployment_tool_sources(tool_source_id);
CREATE INDEX idx_group_tool_sources_group_id ON group_tool_sources(group_id);
CREATE INDEX idx_group_tool_sources_tool_source_id ON group_tool_sources(tool_source_id);
CREATE INDEX idx_tool_call_analytics_analytics_id ON tool_call_analytics(analytics_id);
CREATE INDEX idx_tool_call_analytics_tool_source_id ON tool_call_analytics(tool_source_id);

-- Auto-update updated_at on tool_sources changes.
CREATE TRIGGER tool_sources_updated_at
    BEFORE UPDATE ON tool_sources
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
