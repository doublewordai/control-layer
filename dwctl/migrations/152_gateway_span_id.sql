-- Diagnostic context only: keep historical rows nullable and avoid a backfill.
SET LOCAL lock_timeout = '5s';

ALTER TABLE http_analytics ADD COLUMN gateway_span_id TEXT;

COMMENT ON COLUMN http_analytics.gateway_span_id IS
    'OTel span captured by Outlet; pair with trace_id to locate the retained gateway capture subtree';
