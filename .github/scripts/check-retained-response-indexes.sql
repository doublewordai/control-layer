-- Retained-response rollout preflight (read-only).
--
-- Run this against the target database (or a read replica) before enabling
-- retained-response movement, backfill, or partition retirement. Every check
-- raises an exception on failure, so any nonzero psql exit means the
-- deployment prerequisites are not satisfied. The script only reads
-- PostgreSQL catalogs and content-free lifecycle metadata; it never touches
-- payload tables.
DO $$
DECLARE
    orphan RECORD;
BEGIN
    -- 1. The archive-aware schema generation must be installed.
    IF to_regclass('retained_response_objects') IS NULL
        OR to_regclass('retained_response_buckets') IS NULL
        OR to_regclass('retained_response_request_routes') IS NULL
        OR to_regclass('retained_response_step_routes') IS NULL
        OR to_regclass('retained_response_group_routes') IS NULL
        OR to_regclass('retained_response_resurrection_fences') IS NULL
        OR to_regclass('retention_partition_retirements') IS NULL THEN
        RAISE EXCEPTION 'retained-response schema generation is not installed';
    END IF;
    IF to_regprocedure('retained_response_archive_index_ready(text)') IS NULL
        OR to_regprocedure('ensure_retained_response_partition(date,text)') IS NULL
        OR to_regprocedure('ensure_retained_response_partitions(date,integer)') IS NULL THEN
        RAISE EXCEPTION 'retained-response partition helpers are not installed';
    END IF;

    -- 2. The externally prebuilt candidate index must exist with the exact
    --    reviewed definition and be both ready and valid. The migration-owned
    --    guard validates keys, predicate, and readiness; name equality alone
    --    is not sufficient.
    IF NOT retained_response_archive_index_ready(current_schema()) THEN
        RAISE EXCEPTION 'candidate index idx_requests_batchless_retention_due is missing, invalid, or not ready';
    END IF;

    -- 3. No default partition may exist under the retained parent: PostgreSQL
    --    cannot detach concurrently while one is attached.
    IF EXISTS (
        SELECT 1
        FROM pg_partitioned_table partitioned
        WHERE partitioned.partrelid = 'retained_response_objects'::regclass
          AND partitioned.partdefid <> 0
    ) THEN
        RAISE EXCEPTION 'retained_response_objects must not have a default partition';
    END IF;

    -- 4. Every non-retired bucket must describe an attached child whose name
    --    and daily range bounds exactly match its delete_on date.
    FOR orphan IN
        SELECT bucket.delete_on
        FROM retained_response_buckets bucket
        LEFT JOIN pg_class child ON child.oid = bucket.partition_oid
        LEFT JOIN pg_inherits inheritance ON inheritance.inhrelid = child.oid
        WHERE bucket.state IN ('active', 'retiring')
          AND (
              child.oid IS NULL
              OR child.relname <> bucket.partition_table
              OR bucket.partition_table <>
                  'retained_response_objects_d' || to_char(bucket.delete_on, 'YYYYMMDD')
              OR inheritance.inhparent IS DISTINCT FROM 'retained_response_objects'::regclass
              OR pg_get_expr(child.relpartbound, child.oid) IS DISTINCT FROM format(
                  'FOR VALUES FROM (%L) TO (%L)',
                  bucket.delete_on::text,
                  (bucket.delete_on + 1)::text
              )
          )
    LOOP
        RAISE EXCEPTION
            'retained-response bucket % does not describe an exactly attached daily partition',
            orphan.delete_on;
    END LOOP;

    -- 5. A detach-pending child is acceptable only while its exact unfinished
    --    retirement journal entry exists; anything else is an interrupted or
    --    foreign DDL operation that must be resolved before rollout.
    FOR orphan IN
        SELECT child.relname
        FROM pg_inherits inheritance
        JOIN pg_class child ON child.oid = inheritance.inhrelid
        WHERE inheritance.inhparent = 'retained_response_objects'::regclass
          AND inheritance.inhdetachpending
          AND NOT EXISTS (
              SELECT 1
              FROM retention_partition_retirements journal
              WHERE journal.parent_table = 'retained_response_objects'
                AND journal.partition_oid = child.oid
                AND journal.partition_table = child.relname
                AND journal.completed_at IS NULL
          )
    LOOP
        RAISE EXCEPTION
            'partition % is detach-pending without an unfinished retirement journal',
            orphan.relname;
    END LOOP;

    -- 6. Every attached child must be described by a bucket row; an unmanaged
    --    child would silently escape scheduled retirement.
    FOR orphan IN
        SELECT child.relname
        FROM pg_inherits inheritance
        JOIN pg_class child ON child.oid = inheritance.inhrelid
        WHERE inheritance.inhparent = 'retained_response_objects'::regclass
          AND NOT EXISTS (
              SELECT 1
              FROM retained_response_buckets bucket
              WHERE bucket.partition_oid = child.oid
          )
    LOOP
        RAISE EXCEPTION 'partition % is not described by a lifecycle bucket', orphan.relname;
    END LOOP;
END $$;
