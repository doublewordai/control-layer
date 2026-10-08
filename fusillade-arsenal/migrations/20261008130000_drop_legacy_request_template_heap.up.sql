-- Retire the generation-1 request template heap.
--
-- Remove the legacy heap and collapse the generation-transparent views to
-- generation 2. `active_request_templates` and `request_templates_all`
-- keep their names and column shapes.
--
-- The guard is deliberately conservative and fails closed. It refuses to run
-- (SQLSTATE 55000, object_not_in_prerequisite_state) while any legacy row is
-- still reachable from a live object, or while the legacy heap has received a
-- write at or after the newest generation-2 write, so the drop can never take
-- content that a reader could still resolve. Nothing is deleted row by row;
-- the heap goes as one relation.
--
-- Wait for legacy readers and writers before taking the validation snapshot:
-- an existing reader may still create a request referencing a legacy template.
-- Bound both lock acquisition and validation so a large archive fails closed
-- instead of holding up legacy readers for an unbounded time.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '5s';
LOCK TABLE request_templates IN ACCESS EXCLUSIVE MODE;

DO $$
DECLARE
    blocking_reason TEXT;
BEGIN
    -- Drive probes from references, not from the potentially much larger legacy
    -- heap. LIMIT prevents the planner from flattening the lateral index probes
    -- into a legacy-first scan. Archives are pruned one registered week at a time.
    -- Keep all probes in one statement so concurrent archive moves cannot hide
    -- a reference between separate live and archive snapshots.
    SELECT reason INTO blocking_reason
    FROM (
        SELECT 'a legacy template is still referenced by a live request' AS reason
        WHERE EXISTS (
            SELECT 1
            FROM requests request
            CROSS JOIN LATERAL (
                SELECT 1 FROM request_templates legacy
                WHERE legacy.id = request.template_id
                LIMIT 1
            ) referenced
        )
        UNION ALL
        SELECT 'a legacy template is still referenced by an archived batch request'
        WHERE EXISTS (SELECT 1 FROM request_templates)
          AND EXISTS (
            SELECT 1
            FROM batch_archive_buckets bucket
            CROSS JOIN LATERAL (
                SELECT 1
                FROM batch_requests_archive archived
                CROSS JOIN LATERAL (
                    SELECT 1 FROM request_templates legacy
                    WHERE legacy.id = archived.template_id
                    LIMIT 1
                ) legacy
                WHERE archived.archive_bucket = bucket.week_start
                LIMIT 1
            ) referenced
        )
        UNION ALL
        SELECT 'a legacy template still belongs to a file that is not deleted'
        WHERE EXISTS (
            SELECT 1
            FROM files file
            CROSS JOIN LATERAL (
                SELECT 1 FROM request_templates legacy
                WHERE legacy.file_id = file.id
                LIMIT 1
            ) referenced
            WHERE file.deleted_at IS NULL
        )
        UNION ALL
        SELECT 'the legacy heap holds a template at least as new as the newest generation-2 template'
        WHERE EXISTS (
            SELECT 1
            FROM request_templates legacy
            WHERE (legacy.created_at AT TIME ZONE 'UTC')
                >= COALESCE(
                    (SELECT MAX(created_on) FROM request_templates_g2),
                    DATE '-infinity'
                )
        )
    ) blockers
    LIMIT 1;

    IF blocking_reason IS NOT NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'cannot retire the legacy request template heap: ' || blocking_reason,
            HINT = 'Wait until every retention window that could hold '
                || 'generation-1 content has passed, then rerun the migration.';
    END IF;
END;
$$;

-- Generation-2 only. Dedicated batchless templates (file_id IS NULL) now live
-- here too, so the file join is outer, exactly as the legacy arm's was: a
-- dedicated template stays visible to the claim join, and a file-backed
-- template disappears with its soft-deleted file. The route oracle keeps a
-- point read on one weekly partition, and the bucket fence hides retiring
-- content before any destructive DDL.
CREATE OR REPLACE VIEW active_request_templates AS
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
FROM request_template_routes route
JOIN request_template_buckets bucket
  ON bucket.week_start = route.week_start
 AND bucket.state = 'active'
JOIN request_templates_g2 g2
  ON g2.created_on >= route.week_start
 AND g2.created_on < route.week_start + 7
 AND g2.id = route.template_id
LEFT JOIN files f ON g2.file_id = f.id
WHERE g2.file_id IS NULL OR f.deleted_at IS NULL;

-- Raw union for internal file-keyed reads (statistics, streaming, request
-- materialization). No liveness or fence predicates: it mirrors direct
-- base-table access.
CREATE OR REPLACE VIEW request_templates_all AS
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
FROM request_templates_g2 g2;

DROP TABLE request_templates;

-- The preceding generation-2 writer still reads/deletes legacy templates
-- during cleanup. Keep those queries valid while its processes drain. This
-- automatically updatable view owns no storage, exposes no rows, and cannot
-- accept writes that could become invisible to the route oracle.
CREATE VIEW request_templates AS
SELECT id, file_id, endpoint, method, path, body, model, api_key, created_at,
       updated_at, custom_id, line_number, body_byte_size, metadata
FROM request_templates_g2
WHERE false
WITH CASCADED CHECK OPTION;
