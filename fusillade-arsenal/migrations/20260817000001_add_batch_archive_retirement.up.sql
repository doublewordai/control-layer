-- Weekly batch-archive retirement metadata.
--
-- Expand-only. Registers every weekly `batch_requests_archive` child in a
-- lifecycle registry so archived batch response content can be deleted by
-- journaled whole-partition drops, mirroring the retained-response daily
-- lifecycle. Batch and file metadata rows are never deleted by this
-- lifecycle; only partition content is.

CREATE TABLE batch_archive_buckets (
    week_start DATE PRIMARY KEY
        CHECK (week_start = date_trunc('week', week_start)::date),
    partition_schema TEXT NOT NULL,
    partition_table TEXT NOT NULL,
    partition_oid OID NOT NULL,
    state TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'retiring', 'retired')),
    state_changed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (partition_schema, partition_table)
);

COMMENT ON TABLE batch_archive_buckets IS
    'Read fence and lifecycle registry for weekly batch_requests_archive partitions.';

-- Register the already-attached weekly children. Catalog-only: never touches
-- partition contents. The ISO year/week in each child name converts back to
-- its Monday lower bound.
INSERT INTO batch_archive_buckets (
    week_start, partition_schema, partition_table, partition_oid
)
SELECT
    to_date(substring(child.relname FROM 'y(\d{4})w\d{2}$') || '-'
            || substring(child.relname FROM 'w(\d{2})$') || '-1',
            'IYYY-IW-ID'),
    namespace.nspname,
    child.relname,
    child.oid
FROM pg_inherits inheritance
JOIN pg_class child ON child.oid = inheritance.inhrelid
JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
WHERE inheritance.inhparent = 'batch_requests_archive'::regclass
  AND child.relname ~ '^batch_requests_archive_y\d{4}w\d{2}$'
ON CONFLICT (week_start) DO NOTHING;

-- The runway function now also registers each child it creates, and
-- registers any pre-existing unregistered child it passes over.
CREATE OR REPLACE FUNCTION ensure_archive_partitions(weeks_ahead integer DEFAULT 4)
RETURNS integer
LANGUAGE plpgsql
AS $$
DECLARE
    this_monday date := date_trunc('week', now() AT TIME ZONE 'UTC')::date;
    target date;
    part_name text;
    part_oid oid;
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
        part_oid := to_regclass(part_name);
        INSERT INTO batch_archive_buckets (
            week_start, partition_schema, partition_table, partition_oid
        ) VALUES (target, current_schema(), part_name, part_oid)
        ON CONFLICT (week_start) DO NOTHING;
    END LOOP;
    RETURN created;
END;
$$;

-- The shared retirement journal now also carries the complete weekly
-- identity for batch-archive retirements; bounds-tampered rows remain
-- unrepresentable for both parents.
ALTER TABLE retention_partition_retirements
    DROP CONSTRAINT retained_response_retirement_identity_complete;
ALTER TABLE retention_partition_retirements
    ADD CONSTRAINT retained_response_retirement_identity_complete CHECK (
        (
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
        ) AND (
            parent_table <> 'batch_requests_archive'
            OR (
                partition_schema IS NOT NULL
                AND partition_schema_oid IS NOT NULL
                AND parent_oid IS NOT NULL
                AND lower_bound IS NOT NULL
                AND upper_bound IS NOT NULL
                AND upper_bound = lower_bound + 7
                AND lower_bound = date_trunc('week', lower_bound)::date
                AND partition_table =
                    'batch_requests_archive_y' || to_char(lower_bound, 'IYYY')
                        || 'w' || to_char(lower_bound, 'IW')
            )
        )
    );
