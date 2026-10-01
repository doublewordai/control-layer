SET LOCAL lock_timeout = '5s';

ALTER TABLE files RESET (
    autovacuum_vacuum_scale_factor,
    autovacuum_vacuum_threshold,
    autovacuum_analyze_scale_factor,
    autovacuum_analyze_threshold,
    autovacuum_vacuum_insert_scale_factor,
    autovacuum_vacuum_insert_threshold
);
