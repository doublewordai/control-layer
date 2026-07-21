-- Phase 3 (batch archive) — schema EXPAND only. Additive, instant at boot,
-- invisible to all existing code: no query reads or writes these objects
-- until the (separately shipped, config-gated) sweeper/backfill code is
-- enabled. Deploys never move data; only config flags do.
--
-- Design + rationale: repo-root fusillade-requests-phase3-plan.md and
-- fusillade-phase3-partitioning-decisions.md (clay/core workspace).

-- ============================================================================
-- 1. The archive: terminal batch request rows, RANGE-partitioned by the ISO
--    week (Monday) of the PARENT BATCH's created_at — "archive_bucket".
--    The key is batch-constant: a batch's rows always colocate in exactly one
--    partition, retries re-archive to the same one, partitions age naturally.
--
--    Columns deliberately mirror `requests` 1:1 (via LIKE), with exactly one
--    addition: `archive_bucket`, appended LAST. Moves therefore need no
--    per-column transformation — archiving is literally
--    `INSERT INTO batch_requests_archive SELECT r.*, $bucket FROM requests r`
--    (positional alignment + appended key), and the retry move-back selects
--    the `requests` column list, omitting only `archive_bucket`. That
--    move-back list lives in one place in the move code and is covered by
--    the same schema-parity test. Claim-machinery columns are NULL on terminal rows and
--    NULLs cost one bit each — the slimming lives in the INDEX set (2 here
--    vs 26 on requests), not the columns. Any future migration that changes
--    `requests` columns MUST change this table in the same migration (a
--    schema-parity test enforces this; the correct fix for that test failing
--    is always "mirror the column", never "delete the test").
-- ============================================================================
CREATE TABLE batch_requests_archive (
    LIKE requests INCLUDING CONSTRAINTS,
    archive_bucket DATE NOT NULL
) PARTITION BY RANGE (archive_bucket);

COMMENT ON TABLE batch_requests_archive IS
    'Terminal batch request rows, moved out of `requests` per batch once the '
    'batch is terminal AND its counts are frozen (counts_frozen_at set). Rows '
    'are written once per residence and never UPDATEd here; the only DELETEs '
    'are retry move-backs (failed/canceled rows return to `requests`) and the '
    'orphan purge (right-to-erasure on batch deletion — the purge MUST cover '
    'this table, the erasure payload lives here after archiving). '
    'DELIBERATELY NO FOREIGN KEYS: FK enforcement would take KEY SHARE locks '
    'on referenced batches/request_templates rows during every bulk move, and '
    'an FK to request_templates with ON DELETE SET NULL would make template '
    'purges UPDATE archived rows (write churn in an immutable table). '
    'Integrity holds by construction: rows arrive only via the move '
    'transaction from already-FK-valid live rows, and batches tombstones are '
    'never hard-deleted. Query contract: every query against this table MUST '
    'carry an explicit archive_bucket = $n::DATE predicate (resolved from '
    'batches.archive_bucket first) so partition pruning is guaranteed — see '
    'fusillade-requests-phase3-plan.md §1.';

COMMENT ON COLUMN batch_requests_archive.archive_bucket IS
    'Partition key: Monday of the ISO week of the PARENT BATCH''s created_at '
    '(NOT the request row''s own timestamps — the key must be batch-constant '
    'so a batch colocates in one partition and retries re-archive to the same '
    'one). Always equals batches.archive_bucket for the row''s batch_id. '
    'Compare against DATE values only: a timestamptz comparison implicitly '
    'casts the column and defeats partition pruning.';

-- Both indexes pack their leaves full (btree default fillfactor is 90,
-- reserving page-split room for future inserts): a partition receives a
-- batch's rows exactly once, in one statement, and is never written again in
-- place — ~10% smaller indexes for free. If anything ever starts UPDATE-ing
-- archived rows, this assumption breaks and must be revisited.
CREATE UNIQUE INDEX idx_batch_requests_archive_id
    ON batch_requests_archive (id, archive_bucket)
    WITH (fillfactor = 100);
COMMENT ON INDEX idx_batch_requests_archive_id IS
    'By-id lookups. The partition key must be part of any unique index on a '
    'partitioned table; callers WITH batch context resolve the bucket from '
    'batches.archive_bucket first (single-partition probe). The one '
    'context-free path (admin request-by-id) probes live first, then this '
    'index across partitions (bounded Append of cheap probes — rare path).';

CREATE INDEX idx_batch_requests_archive_download
    ON batch_requests_archive (batch_id, completed_at, id)
    WITH (fillfactor = 100);
COMMENT ON INDEX idx_batch_requests_archive_download IS
    'Result-download pages in keyset order. The cursor is values-based '
    '(completed_at, id) and table-agnostic: identical column values exist in '
    '`requests` before a move and here after it, which is what lets an '
    'in-progress paged download continue seamlessly across the flip with no '
    'repeated or missing rows. Do not change download ORDER BY away from '
    'this keyset.';

-- ============================================================================
-- 2. Routing columns on batches. `batches` itself is never partitioned or
--    split — listing/sorting paths read only this table and are structurally
--    unaffected by row relocation.
-- ============================================================================
ALTER TABLE batches
    ADD COLUMN location TEXT NOT NULL DEFAULT 'live'
        CONSTRAINT batches_location_check
        CHECK (location IN ('live', 'archive', 'split')),
    ADD COLUMN archive_bucket DATE;

-- location is the SOLE routing authority; archive_bucket IS NULL only means
-- "never archived" and must never be used to infer location. The inverse
-- direction IS enforced: a batch cannot claim its rows are (partly) archived
-- without recording which partition they went to. NOT VALID skips the boot
-- scan — every existing row is location='live', trivially satisfying it.
ALTER TABLE batches
    ADD CONSTRAINT batches_archived_have_bucket
    CHECK (location = 'live' OR archive_bucket IS NOT NULL) NOT VALID;

COMMENT ON COLUMN batches.location IS
    'Where this batch''s request rows live: ''live'' = all in `requests` '
    '(every batch until the sweeper moves it); ''archive'' = all in '
    'batch_requests_archive; ''split'' = mid-retry only (completed rows in '
    'the archive, re-pended rows back in `requests` — re-terminalization '
    're-archives and returns this to ''archive''). Transitions are made ONLY '
    'by: the sweeper/backfill move transaction (live->archive), retry '
    '(archive->split), and re-archive (split->archive) — each guarded by '
    'retry_version CAS. Blue/green invariant: data movement is enabled by '
    'CONFIG FLAGS, never by deploys — old-code pods read `requests` '
    'directly, so nothing may move while any pod without location routing '
    'can serve traffic.';

COMMENT ON COLUMN batches.archive_bucket IS
    'Monday of the ISO week of created_at; the archive partition this '
    'batch''s rows belong to. Stamped by the FIRST move (NULL = rows have '
    'never been archived) and immutable afterwards — retries and re-archives '
    'reuse it so a batch can never scatter across partitions. Readers '
    'resolve this BEFORE querying the archive (pruning contract).';

-- ============================================================================
-- 3. Partition maintenance. Ongoing creation uses create -> bounds CHECK ->
--    ATTACH: the matching CHECK lets Postgres prove containment without
--    scanning, and ATTACH takes only SHARE UPDATE EXCLUSIVE on the parent
--    (direct CREATE ... PARTITION OF takes ACCESS EXCLUSIVE and can queue
--    behind long archive reads). There is deliberately NO DEFAULT partition:
--    if a bucket's partition is missing, the sweeper skips the batch (it
--    simply stays in `requests`, fully served, exactly as today) and the
--    partitions-ahead alert fires — graceful degradation instead of a
--    masked failure.
-- ============================================================================
CREATE FUNCTION ensure_archive_partitions(weeks_ahead integer DEFAULT 4)
RETURNS integer
LANGUAGE plpgsql
AS $$
DECLARE
    this_monday date := date_trunc('week', now())::date;
    target date;
    part_name text;
    created integer := 0;
BEGIN
    -- Serialize concurrent callers: every daemon pod runs the maintenance
    -- tick, and two sessions passing the to_regclass() check together would
    -- race to CREATE/ATTACH the same partition and abort with
    -- duplicate_table. Transaction-scoped, so it self-releases on
    -- commit/rollback; uncontended cost is negligible for a daily tick.
    PERFORM pg_advisory_xact_lock(hashtext('ensure_archive_partitions')::bigint);
    FOR i IN 0..weeks_ahead LOOP
        target := this_monday + (i * 7);
        part_name := 'batch_requests_archive_y'
            || to_char(target, 'IYYY') || 'w' || to_char(target, 'IW');
        IF to_regclass(part_name) IS NULL THEN
            EXECUTE format(
                'CREATE TABLE %I (LIKE batch_requests_archive INCLUDING ALL)',
                part_name
            );
            EXECUTE format(
                'ALTER TABLE %I ADD CONSTRAINT %I '
                'CHECK (archive_bucket >= %L AND archive_bucket < %L)',
                part_name, part_name || '_bounds', target, target + 7
            );
            EXECUTE format(
                'ALTER TABLE batch_requests_archive ATTACH PARTITION %I '
                'FOR VALUES FROM (%L) TO (%L)',
                part_name, target, target + 7
            );
            -- The bounds CHECK exists only to make ATTACH scan-free; the
            -- partition bounds now enforce the same thing.
            EXECUTE format(
                'ALTER TABLE %I DROP CONSTRAINT %I',
                part_name, part_name || '_bounds'
            );
            created := created + 1;
        END IF;
    END LOOP;
    RETURN created;
END;
$$;

COMMENT ON FUNCTION ensure_archive_partitions(integer) IS
    'Idempotent: creates any missing weekly archive partitions from the '
    'current ISO week through now + weeks_ahead, via create -> bounds CHECK '
    '-> ATTACH (scan-free, weak parent lock — safe on a busy parent). Called '
    'by the daemon''s daily maintenance tick with weeks_ahead=4 so the next '
    'partition always exists ~a month early; fusillade_archive_partitions_'
    'ahead alerts if the runway shrinks. Safe to call manually any time.';

-- ============================================================================
-- 4. Bootstrap: partitions covering all existing batches plus four weeks of
--    runway. Direct CREATE ... PARTITION OF is fine HERE ONLY — the parent
--    was created in this transaction, nothing else can hold a lock on it.
--    On an empty database (fresh installs, per-test databases) this creates
--    just current week + 4.
-- ============================================================================
DO $$
DECLARE
    start_monday date;
    end_monday date := date_trunc('week', now())::date + 28;
    target date;
    part_name text;
BEGIN
    SELECT COALESCE(date_trunc('week', min(created_at))::date,
                    date_trunc('week', now())::date)
    INTO start_monday
    FROM batches;

    target := start_monday;
    WHILE target <= end_monday LOOP
        part_name := 'batch_requests_archive_y'
            || to_char(target, 'IYYY') || 'w' || to_char(target, 'IW');
        IF to_regclass(part_name) IS NULL THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF batch_requests_archive '
                'FOR VALUES FROM (%L) TO (%L)',
                part_name, target, target + 7
            );
        END IF;
        target := target + 7;
    END LOOP;
END;
$$;
