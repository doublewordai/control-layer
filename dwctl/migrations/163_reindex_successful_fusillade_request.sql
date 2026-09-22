-- no-transaction
--
-- Repair an interrupted concurrent build from migration 153. PostgreSQL leaves
-- an invalid index behind when CREATE INDEX CONCURRENTLY is interrupted; a
-- plain IF NOT EXISTS retry would otherwise skip that invalid index.
REINDEX INDEX CONCURRENTLY uq_http_analytics_fusillade_success;
