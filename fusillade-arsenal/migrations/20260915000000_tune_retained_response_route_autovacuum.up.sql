-- Fail startup promptly if a running (auto)vacuum holds the brief metadata lock.
SET LOCAL lock_timeout = '1s';

-- The route tables hold tens of millions of rows each and the daily
-- retirement purge deletes on the order of a million rows per table. With the
-- default 0.2 scale factor autovacuum only fires after millions of dead rows,
-- so neither table was ever vacuumed in production, and the dead index entries
-- left behind made every date-bounded route probe read far more than it
-- returned. Absolute thresholds keep maintenance proportional to the churn.
--
-- Metadata-only change; takes effect on the next autovacuum cycle.
ALTER TABLE retained_response_group_routes SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 100000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 100000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 100000
);

ALTER TABLE retained_response_request_routes SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 100000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 100000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 100000
);
