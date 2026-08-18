-- Reversible only while no batch-archive retirement has started: a bucket
-- past `active` or an unfinished/completed batch-archive journal row proves
-- durable lifecycle state this revert would abandon.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM batch_archive_buckets WHERE state <> 'active' LIMIT 1
    ) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove batch-archive lifecycle while retirement state exists';
    END IF;
    IF EXISTS (
        SELECT 1 FROM retention_partition_retirements
        WHERE parent_table = 'batch_requests_archive' LIMIT 1
    ) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove batch-archive lifecycle while journal rows exist';
    END IF;
END;
$$;

ALTER TABLE retention_partition_retirements
    DROP CONSTRAINT retained_response_retirement_identity_complete;
ALTER TABLE retention_partition_retirements
    ADD CONSTRAINT retained_response_retirement_identity_complete CHECK (
        parent_table <> 'retained_response_objects'
        OR (
            partition_schema IS NOT NULL
            AND partition_schema_oid IS NOT NULL
            AND parent_oid IS NOT NULL
            AND lower_bound IS NOT NULL
            AND upper_bound IS NOT NULL
            AND upper_bound = lower_bound + 1
            AND partition_table =
                'retained_response_objects_d' || to_char(lower_bound, 'YYYYMMDD')
        )
    );

-- Restore the pre-registry runway function exactly as defined by
-- 20260720000000_fix_ensure_archive_partitions_utc.up.sql.
CREATE OR REPLACE FUNCTION ensure_archive_partitions(weeks_ahead integer DEFAULT 4)
RETURNS integer
LANGUAGE plpgsql
AS $$
DECLARE
    this_monday date := date_trunc('week', now() AT TIME ZONE 'UTC')::date;
    target date;
    part_name text;
    created integer := 0;
BEGIN
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

DROP TABLE batch_archive_buckets;
