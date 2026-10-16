-- Per-minute counters of held batch dispatches (the daemon's dispatch hold).
--
-- Every daemon counts its own held dispatches that completed and those the
-- backend refused (503/529), and adds them here every few seconds; every
-- daemon reads the trailing window back to decide, per held model, which
-- held requests to release. Rows older than two hours are purged by the
-- daemons. Nothing reads or writes requests or the archive for this.
CREATE TABLE IF NOT EXISTS hold_stats (
    model TEXT NOT NULL,
    minute TIMESTAMPTZ NOT NULL,
    completed BIGINT NOT NULL DEFAULT 0 CHECK (completed >= 0),
    refused BIGINT NOT NULL DEFAULT 0 CHECK (refused >= 0),
    PRIMARY KEY (model, minute)
);
