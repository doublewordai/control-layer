-- Tighten autovacuum/autoanalyze and planner statistics for credits_transactions.
--
-- credits_transactions is the append-heavy billing ledger (tens of millions of
-- rows and growing). The onwards config sync recomputes every relevant user's
-- balance as checkpoint + delta, where the delta is an index-only scan over the
-- most recently inserted rows (seq > checkpoint_seq). Postgres's default
-- maintenance settings broke this in two independent ways:
--
--   * Visibility map went stale. The default autovacuum thresholds are a
--     *fraction of the whole table* (scale_factor 0.2 for vacuum/insert), so on
--     a 34M-row table autovacuum only fires after ~6.8M inserts. Between runs the
--     freshly-appended pages -- exactly the ones the delta scan reads -- are never
--     marked all-visible, so the "index-only" scan falls back to the heap. We
--     observed ~90k heap fetches and a 44s sync query; a manual VACUUM (which
--     restored the VM to ~100% all-visible) cut it to sub-second.
--
--   * Planner statistics for user_id were too coarse. The default statistics
--     target (100) sampled too few rows of a skewed 34M-row table and estimated
--     n_distinct(user_id) = 165, so the planner believed each user had ~200k
--     transactions and chose a catastrophic nested-loop cross-product for the
--     balance lookup. Raising the target fixes the estimate and the plan.
--
-- Fix, mirroring fusillade's autovacuum tuning on its high-churn claim tables:
--   1. Switch to absolute autovacuum/autoanalyze thresholds (scale_factor = 0,
--      threshold = N) so maintenance fires after a fixed number of changes
--      regardless of how large the ledger grows, keeping the visibility map
--      current and dead-tuple bloat bounded. The insert-triggered threshold is
--      the one that keeps the VM fresh for the delta scans.
--   2. Raise the statistics target on user_id so ANALYZE estimates n_distinct
--      accurately and the planner keeps choosing the merge join.
--
-- Both statements are metadata-only and take effect on the next autovacuum /
-- ANALYZE cycle. The one-time VACUUM (ANALYZE) that reclaims the existing
-- backlog was run manually against live databases -- VACUUM cannot run inside a
-- migration transaction. On a fresh build there is no backlog: these settings
-- keep the table maintained from the first rows, so this migration is a no-op
-- beyond recording the configuration in source.

ALTER TABLE credits_transactions ALTER COLUMN user_id SET STATISTICS 1000;

ALTER TABLE credits_transactions SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 50000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 50000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 50000
);
