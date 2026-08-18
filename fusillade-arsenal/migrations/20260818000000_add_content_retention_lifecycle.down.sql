-- Reversible only while the whole retention lifecycle is empty: any retained
-- object, route, fence, non-active bucket, or journal row proves durable
-- state this revert would silently abandon, so every guard fails closed.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM request_templates_g2 LIMIT 1) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove generation-2 template storage while template rows exist';
    END IF;
    IF EXISTS (SELECT 1 FROM request_template_routes LIMIT 1) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove generation-2 template storage while routes exist';
    END IF;
    IF EXISTS (
        SELECT 1 FROM request_template_buckets WHERE state <> 'active' LIMIT 1
    ) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove generation-2 template storage while lifecycle state exists';
    END IF;
    IF EXISTS (
        SELECT 1 FROM batch_archive_buckets WHERE state <> 'active' LIMIT 1
    ) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove batch-archive lifecycle while retirement state exists';
    END IF;
    IF EXISTS (SELECT 1 FROM retention_partition_retirements LIMIT 1) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove retention lifecycle while journal rows exist';
    END IF;
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
       ) THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot remove retained response storage while retained state exists';
    END IF;
END;
$$;

-- Batch-archive registry teardown; restore the pre-registry runway function
-- exactly as defined by 20260720000000_fix_ensure_archive_partitions_utc.up.sql.
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

-- Template generation teardown; restore the single-generation view exactly
-- as last defined by 20260731010000_add_request_template_metadata.up.sql.
DROP VIEW IF EXISTS request_templates_all;
DROP VIEW IF EXISTS active_request_templates;
CREATE VIEW active_request_templates AS
SELECT rt.*
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL;

DROP FUNCTION ensure_request_template_partitions(DATE, INTEGER);
DROP FUNCTION ensure_request_template_partition(DATE, TEXT);
DROP TABLE request_template_routes;
DROP TABLE request_template_buckets;
DROP TABLE request_templates_g2;

-- Retained-response teardown.
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
