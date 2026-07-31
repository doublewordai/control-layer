-- Per-request provenance for BATCHLESS requests (flex, background), which have no
-- batch row to hang metadata on.
--
-- Batch work already answers "which client submitted this": control-layer stamps the
-- creating call's User-Agent into the batch's metadata, and the claim path replays every
-- listed metadata key onto each dispatched request as an `x-fusillade-batch-<key>` header.
-- Flex and background have no batch, so that path had nothing to read and their analytics
-- rows carry no client at all - the daemon's own HTTP client sends no User-Agent.
--
-- This column is the batchless equivalent of `batches.metadata`, deliberately shaped the
-- same (JSONB, read through the same allow-list, forwarded under the same header names) so
-- the claim path treats both alike and a future key costs a write site rather than a schema
-- change.
--
-- On `request_templates` rather than `requests`: templates are already 1:1 with batchless
-- requests, the claim queries already join them, and `requests` is the hot table the claim
-- UPDATE locks. Retries reuse the row, so there is nothing per-attempt to lose.
--
-- NULL for every template ingested from a batch file: those requests read their metadata
-- from the parent batch. A nullable add is catalog-only, so no rewrite of a large table.
ALTER TABLE request_templates ADD COLUMN IF NOT EXISTS metadata JSONB;

-- The claim path reads templates through this view, and a view over `SELECT rt.*` FREEZES
-- its column list at creation time: the new column is invisible to it until the view is
-- rebuilt, so `t.metadata` would fail to resolve with the column sitting right there on the
-- table. Same body as 20260507000000, which is also where the `file_id IS NULL` arm comes
-- from - batchless templates have no parent file and must stay visible.
--
-- CREATE OR REPLACE rather than DROP: the existing columns keep their order and types and
-- `metadata` is appended last, which is the one shape Postgres allows a replace to change,
-- and it avoids dropping a view the daemon is actively claiming through.
CREATE OR REPLACE VIEW active_request_templates AS
SELECT rt.*
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL;
