DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass('underway.idx_task_queue_state')
          AND i.indrelid = 'underway.task'::regclass
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 2 AND i.indnatts = 2
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'task_queue_name'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'state'
          AND i.indoption::text = '0 0'
          AND i.indpred IS NULL AND i.indexprs IS NULL
    ) THEN
        RAISE EXCEPTION 'idx_task_queue_state is missing, invalid, or has the wrong definition';
    END IF;
END
$$;

COMMENT ON INDEX underway.idx_task_queue_state IS
    'Supports Underway task claims by queue and state without scanning completed task history.';
