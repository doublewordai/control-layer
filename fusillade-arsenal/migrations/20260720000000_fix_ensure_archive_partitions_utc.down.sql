-- Inverse of the UTC correction: restore the original session-TimeZone week
-- derivation (the definition from 20260716000000 as first released). Only the
-- function body differs.

CREATE OR REPLACE FUNCTION ensure_archive_partitions(weeks_ahead integer DEFAULT 4)
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
