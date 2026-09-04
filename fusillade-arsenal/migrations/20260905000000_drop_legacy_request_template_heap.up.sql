-- Retire the generation-1 request template heap.
--
-- The generation-2 weekly template store (introduced by the content retention
-- lifecycle in 11.2.0) has been the write target for long enough that the
-- frozen generation-1 heap holds no content inside any retention window. This
-- migration removes the legacy heap and the generation-transparent
-- compatibility layer that read both generations: `active_request_templates`
-- and `request_templates_all` keep their names and column shapes but resolve
-- generation-2 rows only.
--
-- The guard is deliberately conservative and fails closed. It refuses to run
-- (SQLSTATE 55000, object_not_in_prerequisite_state) while any legacy row is
-- still reachable from a live object, or while the legacy heap has received a
-- write at or after the newest generation-2 write, so the drop can never take
-- content that a reader could still resolve. Nothing is deleted row by row;
-- the heap goes as one relation.
--
-- DROP TABLE takes ACCESS EXCLUSIVE on the legacy heap and, through the view
-- redefinitions, briefly on the views the claim path reads. Bound the wait so
-- a busy daemon is never queued behind this migration.
SET LOCAL lock_timeout = '5s';

DO $$
DECLARE
    blocking_reason TEXT;
BEGIN
    -- Each probe is driven from the legacy heap, which is expected to be
    -- small or empty by the time this runs; the per-row probes into
    -- `requests` use its template_id index, the archive probe hashes the
    -- heap against one archive pass.
    SELECT reason INTO blocking_reason
    FROM (
        SELECT 'a legacy template is still referenced by a live request' AS reason
        WHERE EXISTS (
            SELECT 1
            FROM request_templates legacy
            WHERE EXISTS (
                SELECT 1 FROM requests request WHERE request.template_id = legacy.id
            )
        )
        UNION ALL
        SELECT 'a legacy template is still referenced by an archived batch request'
        WHERE EXISTS (
            SELECT 1
            FROM request_templates legacy
            JOIN batch_requests_archive archived ON archived.template_id = legacy.id
        )
        UNION ALL
        SELECT 'a legacy template still belongs to a file that is not deleted'
        WHERE EXISTS (
            SELECT 1
            FROM request_templates legacy
            JOIN files file ON file.id = legacy.file_id
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
