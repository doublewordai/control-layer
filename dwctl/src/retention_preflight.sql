SELECT
    (SELECT COUNT(*)::bigint
     FROM retention_partition_retirements
     WHERE completed_at IS NULL),
    EXISTS (
        SELECT 1
        FROM retained_response_buckets bucket
        WHERE bucket.state IN ('retiring', 'retired')
          AND NOT (
            (bucket.state = 'retiring' AND EXISTS (
                SELECT 1
                FROM retention_partition_retirements journal
                JOIN pg_namespace namespace
                  ON namespace.nspname = bucket.partition_schema
                 AND namespace.oid = journal.partition_schema_oid
                JOIN pg_class parent
                  ON parent.relnamespace = namespace.oid
                 AND parent.relname = 'retained_response_objects'
                 AND parent.oid = journal.parent_oid
                JOIN pg_class child
                  ON child.relnamespace = namespace.oid
                 AND child.relname = bucket.partition_table
                 AND child.oid = bucket.partition_oid
                WHERE journal.parent_table = 'retained_response_objects'
                  AND journal.partition_schema = bucket.partition_schema
                  AND journal.partition_table = bucket.partition_table
                  AND journal.partition_oid = bucket.partition_oid
                  AND journal.lower_bound = bucket.delete_on
                  AND journal.upper_bound = bucket.delete_on + 1
                  AND journal.completed_at IS NULL
                  AND bucket.partition_schema = current_schema()
                  AND bucket.partition_table =
                      'retained_response_objects_d'
                      || to_char(bucket.delete_on, 'YYYYMMDD')
            )) OR (bucket.state = 'retired' AND EXISTS (
                SELECT 1
                FROM retention_partition_retirements journal
                JOIN pg_namespace namespace
                  ON namespace.nspname = bucket.partition_schema
                 AND namespace.oid = journal.partition_schema_oid
                JOIN pg_class parent
                  ON parent.relnamespace = namespace.oid
                 AND parent.relname = 'retained_response_objects'
                 AND parent.oid = journal.parent_oid
                WHERE journal.parent_table = 'retained_response_objects'
                  AND journal.partition_schema = bucket.partition_schema
                  AND journal.partition_table = bucket.partition_table
                  AND journal.partition_oid = bucket.partition_oid
                  AND journal.lower_bound = bucket.delete_on
                  AND journal.upper_bound = bucket.delete_on + 1
                  AND journal.completed_at = bucket.state_changed_at
                  AND bucket.partition_schema = current_schema()
                  AND bucket.partition_table =
                      'retained_response_objects_d'
                      || to_char(bucket.delete_on, 'YYYYMMDD')
                  AND NOT EXISTS (
                      SELECT 1 FROM pg_class child
                      WHERE child.oid = bucket.partition_oid
                  )
            ))
          )
    ) OR EXISTS (
        SELECT 1
        FROM retention_partition_retirements journal
        WHERE journal.parent_table = 'retained_response_objects'
          AND journal.completed_at IS NULL
          AND NOT EXISTS (
              SELECT 1
              FROM retained_response_buckets bucket
              JOIN pg_namespace namespace
                ON namespace.nspname = bucket.partition_schema
               AND namespace.oid = journal.partition_schema_oid
              JOIN pg_class parent
                ON parent.relnamespace = namespace.oid
               AND parent.relname = 'retained_response_objects'
               AND parent.oid = journal.parent_oid
              JOIN pg_class child
                ON child.relnamespace = namespace.oid
               AND child.relname = bucket.partition_table
               AND child.oid = bucket.partition_oid
              WHERE bucket.state = 'retiring'
                AND bucket.partition_schema = journal.partition_schema
                AND bucket.partition_table = journal.partition_table
                AND bucket.partition_oid = journal.partition_oid
                AND bucket.delete_on = journal.lower_bound
                AND journal.upper_bound = journal.lower_bound + 1
                AND bucket.partition_schema = current_schema()
                AND bucket.partition_table =
                    'retained_response_objects_d'
                    || to_char(bucket.delete_on, 'YYYYMMDD')
          )
    ),
    EXISTS (
        SELECT 1 FROM retained_response_buckets bucket
        JOIN retention_partition_retirements journal
          ON journal.parent_table = 'retained_response_objects'
         AND journal.partition_schema = bucket.partition_schema
         AND journal.partition_table = bucket.partition_table
         AND journal.partition_oid = bucket.partition_oid
         AND journal.lower_bound = bucket.delete_on
         AND journal.upper_bound = bucket.delete_on + 1
         AND journal.completed_at = bucket.state_changed_at
        WHERE bucket.state = 'retired'
          AND journal.completed_at IS NOT NULL
          AND (
              -- EXISTS can favor a sequential scan expecting an early match, but
              -- retired dates have no routes. Preserve ordering and LIMIT in a
              -- scalar subquery so the (delete_on, id) indexes bound each probe.
              (SELECT route.group_id FROM retained_response_group_routes route
               WHERE route.delete_on = bucket.delete_on
               ORDER BY route.group_id LIMIT 1) IS NOT NULL
              OR (SELECT route.request_id FROM retained_response_request_routes route
                  WHERE route.delete_on = bucket.delete_on
                  ORDER BY route.request_id LIMIT 1) IS NOT NULL
          )
    )
