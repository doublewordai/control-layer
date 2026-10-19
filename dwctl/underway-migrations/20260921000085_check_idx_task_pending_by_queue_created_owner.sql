-- The next migration drops a hand-built index that some installations carry.
-- DROP INDEX CONCURRENTLY cannot run in a transaction, so it cannot check
-- anything first; this step fails early, with an actionable message, when
-- the index exists but the migration role cannot drop it. Reassign or drop
-- it by hand, then rerun.
DO $$
DECLARE
    owner text;
BEGIN
    SELECT pg_get_userbyid(c.relowner) INTO owner
    FROM pg_class c
    WHERE c.oid = to_regclass('underway.idx_task_pending_by_queue_created')
      AND NOT pg_has_role(current_user, c.relowner, 'USAGE');
    IF owner IS NOT NULL THEN
        RAISE EXCEPTION 'underway.idx_task_pending_by_queue_created is owned by % and cannot be dropped as %; run ALTER INDEX underway.idx_task_pending_by_queue_created OWNER TO % (or drop it) before migrating',
            owner, current_user, current_user;
    END IF;
END
$$;
