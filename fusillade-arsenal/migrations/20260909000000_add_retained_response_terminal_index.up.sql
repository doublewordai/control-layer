-- Index terminal_at on the retained-response store.
--
-- The trailing terminal-demand query (TRAILING_DEMAND_SQL, served to scouter
-- via /monitoring/demand) filters retained request objects by a trailing
-- terminal_at window. Partition pruning on delete_on limits WHICH daily
-- partitions are read, but inside an admitted partition no existing index
-- leads on terminal_at (they lead on created_at), so every row is filtered
-- one by one. While the admitted partitions were empty that was invisible; once
-- the retention mover landed a full day of terminals (~1.8M rows) into a
-- partition the window admits, the statement ran to its 60 s timeout on every
-- poll and scouter stopped scheduling (2026-09-08 22:42 UTC). In steady state
-- the current day's landing partition is always inside the window, so this is
-- structural, not a backfill transient.
--
-- (state, terminal_at) turns the filter into an index range: the query
-- constrains state to a single value ('completed' / 'failed') and terminal_at
-- to the window, so the scan visits only in-window rows.
--
-- PRODUCTION: the child indexes were built out of band with CREATE INDEX
-- CONCURRENTLY on 2026-09-09 and attached to a parent index of this exact name
-- and definition, so the statement below is a no-op there (IF NOT EXISTS). Do
-- not let this migration build the index against a live production parent:
-- CREATE INDEX on a partitioned table holds SHARE on the parent for the whole
-- build, which blocks the movers' inserts on every partition until it commits.
-- On fresh, development and forked databases the partitions are small and the
-- build is immediate. Partitions created later by
-- ensure_retained_response_partition() inherit it (LIKE ... INCLUDING ALL, then
-- ATTACH PARTITION).
SET LOCAL lock_timeout = '5s';

CREATE INDEX IF NOT EXISTS idx_retained_response_objects_state_terminal
    ON retained_response_objects (state, terminal_at)
    WHERE object_kind = 'request';

COMMENT ON INDEX idx_retained_response_objects_state_terminal IS
'Trailing terminal-demand window scans: (state, terminal_at) on retained request objects. Existing indexes lead on created_at; without this the demand query filters every row of each admitted daily partition.';
