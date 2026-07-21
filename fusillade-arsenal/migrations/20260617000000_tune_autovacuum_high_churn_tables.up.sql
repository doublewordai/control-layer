-- Tighten autovacuum / autoanalyze for the high-churn claim tables.
--
-- `requests` and `batches` change constantly: every request moves
-- pending -> claimed -> processing -> terminal, and batches are created and
-- completed continuously. Postgres's default autovacuum/autoanalyze scale
-- factors (0.2 / 0.1) are a *fraction of the whole table*, so the larger these
-- tables get, the more dead/changed rows must accumulate before maintenance
-- fires. Two failure modes follow on these specific tables:
--
--   * planner statistics go stale, so the claim query mis-estimates row counts
--     and stops choosing its purpose-built partial indexes (idx_requests_pending*),
--     falling back to a broad state-only index scan; and
--   * dead tuples accumulate, which bloats indexes and forces the claim's
--     index-only scans to fall back to heap fetches.
--
-- Switch both tables to absolute thresholds (scale_factor = 0, threshold = N)
-- so autovacuum and autoanalyze fire after a fixed number of changes regardless
-- of how large the table grows, keeping statistics fresh and dead-tuple bloat
-- bounded. We pin the insert-triggered thresholds (autovacuum_vacuum_insert_*)
-- on both tables as well: both are insert-heavy (requests are inserted in bulk
-- at batch creation; batches are created continuously), and insert-triggered
-- vacuums are what freeze tuples and keep the visibility map current so the
-- claim's index-only scans don't fall back to heap fetches.
--
-- `requests` uses a higher absolute threshold (50k) than `batches` (10k): it is
-- the far larger, higher-volume table (bulk request inserts plus per-claim state
-- churn), so a higher bound avoids vacuuming it excessively; the smaller
-- `batches` table gets a tighter bound to keep its statistics fresh.
--
-- Metadata-only change; takes effect on the next autovacuum cycle.

ALTER TABLE requests SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 50000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 50000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 50000
);

ALTER TABLE batches SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 10000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 10000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 10000
);
