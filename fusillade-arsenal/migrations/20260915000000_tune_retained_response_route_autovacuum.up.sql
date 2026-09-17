-- Fail startup promptly if a running (auto)vacuum holds the brief metadata lock.
SET LOCAL lock_timeout = '1s';

-- The route tables hold tens of millions of rows each and the daily
-- retirement purge deletes on the order of a million rows per table. With the
-- default 0.2 scale factor autovacuum only fires after millions of dead rows,
-- so neither table was ever vacuumed in production, and the dead index entries
-- left behind made every date-bounded route probe read far more than it
-- returned. Absolute thresholds keep maintenance proportional to the churn.
--
-- Analyze is deliberately looser. Each day adds one date and removes one from
-- a table spanning many, so the statistics barely move, and the retention
-- queries are shaped to hold their index-bounded plans without an estimate.
-- Every analyze samples ~30k random pages, which on Neon is remote IO.
--
-- Metadata-only change; takes effect on the next autovacuum cycle.
ALTER TABLE retained_response_group_routes SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 100000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 500000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 100000
);

ALTER TABLE retained_response_request_routes SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 100000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 500000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 100000
);
