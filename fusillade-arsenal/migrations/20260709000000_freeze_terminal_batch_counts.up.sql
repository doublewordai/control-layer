-- Phase 2 of the requests-table refactor: freeze terminal batch counts.
--
-- Batch status is discovered lazily: reads recount the batch's request rows
-- and stamp the terminal timestamps when the count proves completion. Until
-- now the computed counts were discarded, so every subsequent read recounted
-- an immutable batch forever (the dominant cost of get_batch / list_batches
-- on large histories, and the blocker for archiving terminal rows out of
-- `requests`).
--
-- These columns store the final counts. Frozen ⇔ counts_frozen_at IS NOT
-- NULL; until then the values are meaningless (0) and reads must keep
-- counting live. An explicit marker (rather than inferring from the terminal
-- timestamps) because cancellation sets cancelled_at while in-flight rows
-- are still transitioning — counts only stabilise once every row is in an
-- actual terminal state, which for cancelled batches is after the stamp.
-- pending/in-progress are definitionally 0 for a frozen batch and are not
-- stored.
--
-- History: trigger/WAL-maintained counter columns existed once
-- (20250107000008) and were removed along with their race-prone maintenance
-- (20250118000000_remove_wal_use_on_demand_counting). This brings back
-- storage only — no triggers; the writers are the terminal-stamping UPDATEs
-- (lazy finalization on reads, the notification poller).
--
-- Existing databases: batches that terminalized BEFORE this migration keep
-- recounting until frozen by the one-off, throttled, idempotent side-script
-- backfill-frozen-counts.sh (internal/scripts; run once per DB, safe to
-- kill and rerun). Fresh databases never need it. The script's SQL is
-- covered by fusillade-arsenal's test_backfill_frozen_counts.
ALTER TABLE batches
    ADD COLUMN IF NOT EXISTS completed_requests BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS failed_requests BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS canceled_requests BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS counts_frozen_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS retry_version BIGINT NOT NULL DEFAULT 0;

COMMENT ON COLUMN batches.retry_version IS
    'Optimistic-concurrency token guarding terminal stamping and count freezing against concurrent batch retry. Retry increments it in the same UPDATE that resets terminal state; every stamping/freezing write carries AND retry_version = <value captured at read time>, so a write computed before a retry cannot land after it. Needed because a retried row is otherwise indistinguishable from a never-stamped one (all timestamps NULL, counters 0), and Postgres lock-wait qual re-checks (EvalPlanQual) only re-evaluate target-row columns — subquery guards keep the old snapshot. ANY future writer that un-terminalizes a batch must bump this. Not related to LLM generations; named retry_version deliberately.';

COMMENT ON COLUMN batches.counts_frozen_at IS
    'When set, the *_requests counter columns hold the final frozen counts and reads must not recount from requests. Set once every request row is in an actual terminal state; cleared by batch retry.';

COMMENT ON COLUMN batches.completed_requests IS
    'Frozen count, valid only once a terminal timestamp (completed_at/failed_at/cancelled_at) is set; 0 and unused while the batch is active. Written atomically with the terminal stamp; cleared by batch retry.';
COMMENT ON COLUMN batches.failed_requests IS
    'Frozen count, valid only once a terminal timestamp is set. See completed_requests.';
COMMENT ON COLUMN batches.canceled_requests IS
    'Frozen count, valid only once a terminal timestamp is set. See completed_requests.';
