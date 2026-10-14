-- Correlation key only: keep historical rows nullable and avoid a backfill.
-- A lookup index is a separate CONCURRENTLY build, added when needed.
SET LOCAL lock_timeout = '5s';

ALTER TABLE http_analytics ADD COLUMN x_request_id UUID;

COMMENT ON COLUMN http_analytics.x_request_id IS
    'Request ID minted by dwctl and returned to the client as X-Request-ID; also logged as x_request_id';
