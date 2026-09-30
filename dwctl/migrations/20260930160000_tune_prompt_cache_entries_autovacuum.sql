-- Vacuum and analyze prompt_cache_entries after a fixed number of changes instead of a
-- fraction of the table.
--
-- Every cache read slides its entry's expiry. expires_at is indexed, so each refresh is a
-- non-HOT update that leaves a dead tuple and new entries in every index; the retention
-- daemon then deletes entries in bulk once they are past the recompute grace. With the
-- default thresholds (scale_factor 0.2) a table holding a long cache history waits for
-- tens of millions of dead tuples between runs, and the heap and indexes bloat meanwhile.
--
-- The threshold is deliberately larger than credits_transactions' (migration 108): every
-- run also scans this table's three indexes, which are large, so it is set to
-- run a few times a day at steady state and roughly every half hour while the retention
-- daemon clears an existing backlog at its default pace. The statement is metadata-only
-- and takes effect on the next autovacuum cycle.

ALTER TABLE prompt_cache_entries SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 1000000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 1000000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 1000000
);
