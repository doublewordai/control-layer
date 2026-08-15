DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM retained_response_group_routes LIMIT 1)
       OR EXISTS (SELECT 1 FROM retained_response_request_routes LIMIT 1)
       OR EXISTS (SELECT 1 FROM retained_response_step_routes LIMIT 1)
       OR EXISTS (SELECT 1 FROM retained_response_resurrection_fences LIMIT 1)
       OR EXISTS (SELECT 1 FROM retained_response_objects LIMIT 1)
       OR EXISTS (
           SELECT 1
           FROM retained_response_buckets
           WHERE state IN ('active', 'retiring')
           LIMIT 1
       )
       OR EXISTS (SELECT 1 FROM retention_partition_retirements LIMIT 1) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove retained response storage while retained state exists';
    END IF;
END;
$$;

DROP FUNCTION ensure_retained_response_partitions(DATE, INTEGER);
DROP FUNCTION ensure_retained_response_partition(DATE, TEXT);
DROP FUNCTION retained_response_archive_index_ready(TEXT);
DROP TABLE retained_response_step_routes;
DROP TABLE retained_response_request_routes;
DROP TABLE retained_response_group_routes;
DROP TABLE retained_response_resurrection_fences;
DROP TABLE retention_partition_retirements;
DROP TABLE retained_response_buckets;
DROP TABLE retained_response_objects;
ALTER TABLE batches DROP COLUMN retention_expired_at;
ALTER TABLE files DROP COLUMN retention_expired_at;
