-- Remove request routes whose daily bucket has already been retired, fencing
-- every identifier before its route disappears.
--
-- The retired days are materialised first and each one is probed through the
-- (delete_on, request_id) index. Joining the route table straight to the
-- bucket and journal tables leaves the access path to a generic estimate of
-- routes per day, which can turn every run into a scan of the whole table
-- even though a retired day normally has no routes left.
--
-- $1: bound on the number of routes handled by this statement
-- $2: fence period in seconds, counted from the retirement completion
WITH retired AS MATERIALIZED (
    SELECT bucket.delete_on, journal.completed_at
    FROM retained_response_buckets bucket
    JOIN retention_partition_retirements journal
      ON journal.parent_table = 'retained_response_objects'
     AND journal.partition_schema = bucket.partition_schema
     AND journal.partition_table = bucket.partition_table
     AND journal.partition_oid = bucket.partition_oid
     AND journal.lower_bound = bucket.delete_on
     AND journal.upper_bound = bucket.delete_on + 1
     AND journal.completed_at IS NOT NULL
     AND journal.completed_at = bucket.state_changed_at
    JOIN pg_namespace namespace
      ON namespace.nspname = bucket.partition_schema
     AND namespace.oid = journal.partition_schema_oid
    JOIN pg_class parent
      ON parent.relnamespace = namespace.oid
     AND parent.relname = 'retained_response_objects'
     AND parent.oid = journal.parent_oid
    WHERE bucket.state = 'retired'
      AND bucket.partition_schema = current_schema()
      AND bucket.partition_table = 'retained_response_objects_d'
            || to_char(bucket.delete_on, 'YYYYMMDD')
    ORDER BY bucket.delete_on
), candidates AS MATERIALIZED (
    SELECT route.request_id AS object_id, retired.completed_at
    FROM retired
    CROSS JOIN LATERAL (
        SELECT route.request_id
        FROM retained_response_request_routes route
        WHERE route.delete_on = retired.delete_on
        ORDER BY route.request_id
        FOR UPDATE OF route SKIP LOCKED
        LIMIT $1
    ) route
    ORDER BY retired.delete_on, route.request_id
    LIMIT $1
), fenced AS (
    INSERT INTO retained_response_resurrection_fences (
        object_id, reason, expires_at
    )
    SELECT object_id, 'retired',
           completed_at + ($2::bigint * INTERVAL '1 second')
    FROM candidates
    ON CONFLICT (object_id) DO UPDATE
    SET reason = CASE
            WHEN retained_response_resurrection_fences.reason = 'erased'
                THEN 'erased'
            ELSE 'retired'
        END,
        expires_at = GREATEST(
            retained_response_resurrection_fences.expires_at,
            EXCLUDED.expires_at
        )
    RETURNING object_id
), removed AS (
    -- Probing the primary key with the fenced identifiers keeps the delete on
    -- the index. Joined to the fenced set instead, a batch-sized estimate
    -- makes the planner hash the entire route table.
    DELETE FROM retained_response_request_routes route
    WHERE route.request_id = ANY (ARRAY(SELECT object_id FROM fenced))
    RETURNING 1
)
SELECT COUNT(*)::bigint FROM removed
