-- Fail promptly if a running anti-wraparound vacuum holds the metadata lock; a
-- regular autovacuum is cancelled by the waiting lock after deadlock_timeout.
SET LOCAL lock_timeout = '5s';

-- `files` holds every uploaded and generated file for the retention window,
-- so it grows to millions of rows while content expiry and batch completion
-- keep updating recent ones. With the default 0.2 scale factor autovacuum
-- waits for millions of dead rows, so dead index entries and a stale
-- visibility map slow the expiry and retention scans that walk it. Absolute
-- thresholds keep maintenance proportional to the churn: at the rates a large
-- deployment sees, roughly an hourly vacuum that only scans changed pages.
--
-- Metadata-only change; takes effect on the next autovacuum cycle.
ALTER TABLE files SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 20000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 20000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 20000
);
