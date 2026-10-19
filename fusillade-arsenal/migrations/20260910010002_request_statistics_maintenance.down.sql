DROP TABLE request_statistics_maintenance;
ALTER TABLE requests SET (
    autovacuum_analyze_scale_factor = 0.0,
    autovacuum_analyze_threshold = 50000,
    autovacuum_vacuum_scale_factor = 0.0,
    autovacuum_vacuum_threshold = 50000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold = 50000
);
