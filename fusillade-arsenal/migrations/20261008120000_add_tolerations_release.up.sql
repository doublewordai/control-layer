-- Release of scheduling tolerations on an SLA projection.
--
-- requests.dispatched_tolerated records, at claim, whether the daemon sends
-- the request with its scheduling tolerations (TRUE) or releases them
-- (FALSE: the backend may schedule it anywhere). NULL: no decision
-- (tolerations not configured, background, realtime, or claimed before this
-- migration). Only rows dispatched with tolerations measure the throughput
-- available to tolerated requests: a released one may have been served
-- elsewhere.
--
-- model_release_cutoffs holds one row per model with outstanding work,
-- written by whichever daemon computes the cutoffs (one per refresh
-- interval). The claim releases the tolerations of a request due before its
-- model's release_before_deadline; a NULL cutoff releases nothing beyond the
-- deadline floor. The claim reads it by primary key.
--
-- Nullable column without a default: a metadata-only change. lock_timeout
-- keeps a blocked migration from queueing requests traffic behind it.
SET LOCAL lock_timeout = '5s';

ALTER TABLE requests ADD COLUMN IF NOT EXISTS dispatched_tolerated BOOLEAN;

-- The archive mirrors requests column for column (archive_schema_parity).
-- Appended to the partitioned parent, so it lands after archive_bucket: the
-- forward move therefore names its columns instead of relying on
-- `SELECT r.*, $bucket` alignment, and the parity test compares the column
-- sequences with archive_bucket set aside.
ALTER TABLE batch_requests_archive ADD COLUMN IF NOT EXISTS dispatched_tolerated BOOLEAN;

CREATE TABLE IF NOT EXISTS model_release_cutoffs (
    model TEXT PRIMARY KEY,
    release_before_deadline TIMESTAMPTZ,
    throughput DOUBLE PRECISION NOT NULL CHECK (throughput >= 0),
    backlog_requests BIGINT NOT NULL CHECK (backlog_requests >= 0),
    samples BIGINT NOT NULL CHECK (samples >= 0),
    computed_at TIMESTAMPTZ NOT NULL
);
