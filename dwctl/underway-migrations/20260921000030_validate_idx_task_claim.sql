DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass('underway.idx_task_claim')
          AND i.indrelid = 'underway.task'::regclass
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 4 AND i.indnatts = 4
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'task_queue_name'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'priority'
          AND pg_get_indexdef(i.indexrelid, 3, true) = 'created_at'
          AND pg_get_indexdef(i.indexrelid, 4, true) = 'id'
          -- priority DESC NULLS FIRST (3), the rest ASC (0)
          AND i.indoption::text = '0 3 0 0'
          AND pg_get_expr(i.indpred, i.indrelid) ~
              '^\(state = ANY \(ARRAY\[''pending''::(underway\.)?task_state, ''in_progress''::(underway\.)?task_state\]\)\)$'
          AND i.indexprs IS NULL
    ) THEN
        RAISE EXCEPTION 'idx_task_claim is missing, invalid, or has the wrong definition';
    END IF;
END
$$;

COMMENT ON INDEX underway.idx_task_claim IS
    'Serves Underway task claims in claim order over live (pending/in_progress) rows only, so a claim stops at the first eligible task.';
