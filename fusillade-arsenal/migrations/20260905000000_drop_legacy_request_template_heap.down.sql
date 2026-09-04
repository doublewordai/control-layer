-- Rollback boundary for retiring the generation-1 request template heap.
--
-- Recreates an EMPTY legacy `request_templates` relation with its previous
-- column set, key, indexes, file reference and updated_at trigger, and
-- restores the two-arm generation-transparent views exactly as
-- 20260818000000_add_content_retention_lifecycle.up.sql defined them. Rows
-- dropped by the forward migration are not restored: the forward guard only
-- allowed the drop once nothing live referenced them.
SET LOCAL lock_timeout = '5s';

CREATE TABLE request_templates (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_id UUID REFERENCES files(id) ON DELETE SET NULL,
    endpoint TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL,
    api_key TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    custom_id TEXT,
    line_number INTEGER NOT NULL DEFAULT 0,
    body_byte_size BIGINT NOT NULL DEFAULT 0,
    metadata JSONB
);

CREATE INDEX idx_request_templates_file_id ON request_templates (file_id);
CREATE INDEX idx_request_templates_custom_id ON request_templates (custom_id);
CREATE INDEX idx_request_templates_file_line ON request_templates (file_id, line_number);
CREATE INDEX idx_request_templates_file_model ON request_templates (file_id, model);
CREATE INDEX idx_request_templates_model ON request_templates (model);

CREATE TRIGGER update_request_templates_updated_at
    BEFORE UPDATE ON request_templates
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Two-arm views, verbatim from the retention lifecycle migration.
CREATE OR REPLACE VIEW active_request_templates AS
SELECT rt.id, rt.file_id, rt.endpoint, rt.method, rt.path, rt.body, rt.model,
       rt.api_key, rt.created_at, rt.updated_at, rt.custom_id, rt.line_number,
       rt.body_byte_size, rt.metadata
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL
UNION ALL
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
FROM request_template_routes route
JOIN request_template_buckets bucket
  ON bucket.week_start = route.week_start
 AND bucket.state = 'active'
JOIN request_templates_g2 g2
  ON g2.created_on >= route.week_start
 AND g2.created_on < route.week_start + 7
 AND g2.id = route.template_id
JOIN files f ON g2.file_id = f.id
WHERE f.deleted_at IS NULL;

CREATE OR REPLACE VIEW request_templates_all AS
SELECT rt.id, rt.file_id, rt.endpoint, rt.method, rt.path, rt.body, rt.model,
       rt.api_key, rt.created_at, rt.updated_at, rt.custom_id, rt.line_number,
       rt.body_byte_size, rt.metadata
FROM request_templates rt
UNION ALL
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
FROM request_templates_g2 g2;
