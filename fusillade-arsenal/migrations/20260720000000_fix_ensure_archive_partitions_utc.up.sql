-- Forward-only correction of ensure_archive_partitions()'s week derivation to
-- UTC.
--
-- WHY THIS IS A SEPARATE MIGRATION (do not fold back into 20260716000000):
-- 20260716000000 shipped in an earlier release and was applied to prod (and
-- therefore every daily staging fork). A later change edited that migration
-- file in place to add `AT TIME ZONE 'UTC'`, which changed its sqlx checksum
-- and made every already-migrated database refuse to boot
-- ("migration 20260716000000 was previously applied but has been modified").
-- 20260716000000 has since been reverted to its exact original bytes so the
-- checksum matches the ledger again. The UTC fix is delivered HERE instead,
-- forward-only: sqlx never re-runs an applied migration, so an in-place edit
-- could never have reached an existing database anyway — those DBs still run
-- the session-TZ function until this CREATE OR REPLACE lands.
--
-- The bug: date_trunc('week', now()) truncates in the session TimeZone. A
-- daemon connecting with a non-UTC TimeZone would compute a different Monday
-- than archive_batch's bucket derivation (which is UTC), so a row's
-- archive_bucket and its target partition could disagree at a week boundary.
-- Both sides must derive the ISO-week Monday in UTC.
--
-- Only the function is replaced. The original bootstrap partitions already
-- exist on migrated DBs; future partitions are created by the daemon's daily
-- maintenance tick, which calls this function — so replacing the definition is
-- sufficient. Not called here on purpose: every booting pod runs the migrator,
-- but a migration runs once per DB, and partition creation is the daemon's job.

CREATE OR REPLACE FUNCTION ensure_archive_partitions(weeks_ahead integer DEFAULT 4)
RETURNS integer
LANGUAGE plpgsql
AS $$
DECLARE
    -- UTC, matching archive_batch's bucket derivation exactly: a session
    -- with a non-UTC TimeZone must never compute a different Monday.
    this_monday date := date_trunc('week', now() AT TIME ZONE 'UTC')::date;
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
