-- Per-daemon, per-model throughput sums for the SLA-projection release of
-- spillover tolerations. Each daemon upserts its exponentially decayed sums
-- every refresh interval and reads back the sum over rows refreshed recently,
-- so every replica projects from the same deployment-wide estimate.
--
-- Small and short-lived: one row per (daemon, model) with tolerated traffic.
-- A dead daemon's rows stop being refreshed, drop out of the sums once
-- stale, and are deleted an hour after their last refresh.
CREATE TABLE dispatch_throughput_samples (
    daemon_id UUID NOT NULL,
    model TEXT NOT NULL,
    -- Decayed successful completions of requests dispatched with tolerations.
    completions_decayed DOUBLE PRECISION NOT NULL CHECK (completions_decayed >= 0),
    -- Decayed in-flight-seconds of those requests (same time constant).
    slot_seconds_decayed DOUBLE PRECISION NOT NULL CHECK (slot_seconds_decayed >= 0),
    -- Successful tolerated completions since the daemon started (undecayed).
    samples BIGINT NOT NULL CHECK (samples >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (daemon_id, model)
);

CREATE INDEX idx_dispatch_throughput_samples_updated_at
    ON dispatch_throughput_samples (updated_at);
