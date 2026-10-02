-- Vacuum the high-churn tables after a fixed number of changes instead of a
-- fraction of the table.
--
-- Postgres's default thresholds are a fraction of the whole table
-- (scale_factor 0.2), so on large tables autovacuum waits for hundreds of
-- thousands to millions of dead rows. Between runs the dead rows bloat the
-- indexes and the visibility map goes stale, so index lookups fall back to
-- heap fetches. Each table below churns continuously:
--
--   batch_aggregates             upserted once per batch credit transaction
--   image_access                 upserted (last_seen_at) on every image use
--   batch_capacity_reservations  inserted per reservation, updated on release
--   http_analytics               insert-only request log
--
-- Absolute thresholds keep maintenance proportional to that churn. At the
-- write rates a large deployment sees, each table is vacuumed roughly every
-- half hour to an hour, and each run only scans pages changed since the last.
-- batch_aggregates churns fastest, so it gets the larger bound.
--
-- http_analytics only receives inserts, so only the insert-triggered vacuum
-- is pinned: it keeps the visibility map current for index-only reads and
-- freezes new pages as they fill, instead of leaving them all to one
-- anti-wraparound scan of the whole table. Its analyze stays on the default:
-- the statistics of an append-only log move slowly, and sampling a very large
-- table is expensive.
--
-- ALTER TABLE ... SET waits for a running vacuum of the same table. A regular
-- autovacuum is cancelled by the waiting lock after deadlock_timeout (1 s by
-- default); an anti-wraparound vacuum is not, so fail after a few seconds
-- rather than hold the migration for the length of that vacuum. Re-run once it
-- finishes.
--
-- Metadata-only change; takes effect on the next autovacuum cycle.
SET LOCAL lock_timeout = '5s';

ALTER TABLE batch_aggregates SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 50000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 50000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 50000
);

ALTER TABLE image_access SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 20000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 20000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 20000
);

ALTER TABLE batch_capacity_reservations SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 20000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 20000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 20000
);

ALTER TABLE http_analytics SET (
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 100000
);
