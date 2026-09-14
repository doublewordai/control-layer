-- Fail startup promptly if a busy relation prevents the brief metadata lock.
SET LOCAL lock_timeout = '1s';

-- The active request table can become small after archival. A 50k-change
-- threshold then permits several complete turnovers before an automatic analyze.
ALTER TABLE requests SET (
    autovacuum_analyze_scale_factor = 0.0,
    autovacuum_analyze_threshold = 1000,
    autovacuum_vacuum_scale_factor = 0.0,
    autovacuum_vacuum_threshold = 5000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold = 5000
);

-- One durable cooldown per schema, shared by all archive workers. An incomplete
-- attempt expires automatically; no session locks survive pooled connections.
CREATE TABLE request_statistics_maintenance (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    attempted_at timestamptz NOT NULL DEFAULT '-infinity',
    completed_at timestamptz
);
INSERT INTO request_statistics_maintenance (singleton) VALUES (true);
