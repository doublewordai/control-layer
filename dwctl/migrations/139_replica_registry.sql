-- Live-replica registry for dividing per-role connection budgets across
-- autoscaled pods. Postgres is the consensus substrate (same trust model as the
-- advisory-lock leader election): a row is alive iff its heartbeat is within
-- the liveness window, so every replica of a group converges on the same count
-- within one heartbeat interval. Crashed pods never delete their row; the
-- heartbeat loop sweeps rows that have been silent for a long time.
CREATE TABLE replica_registry (
    instance_id UUID PRIMARY KEY,
    replica_group TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_heartbeat TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_replica_registry_group_heartbeat
    ON replica_registry (replica_group, last_heartbeat DESC);
