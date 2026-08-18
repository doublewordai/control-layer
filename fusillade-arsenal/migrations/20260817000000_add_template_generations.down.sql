-- Reversible only while the generation-2 template lifecycle is empty. Any
-- generation-2 template row, route, or non-empty bucket registry means data
-- or durable lifecycle state would be silently abandoned by reverting, so
-- fail closed instead.
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
END;
$$;

-- Restore the single-generation view exactly as last defined by
-- 20260731010000_add_request_template_metadata.up.sql.
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
