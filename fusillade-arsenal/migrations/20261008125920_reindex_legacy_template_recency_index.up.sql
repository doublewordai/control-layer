-- no-transaction
-- Repair an invalid index left by an interrupted concurrent build. As with
-- the other concurrent-index migrations, a clean build takes a second pass.
REINDEX INDEX CONCURRENTLY idx_request_templates_retirement_created_at;
