DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass('underway.idx_task_terminal_created_at')
          AND i.indrelid = 'underway.task'::regclass
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 1 AND i.indnatts = 1
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_at'
          AND i.indoption::text = '0'
          AND pg_get_expr(i.indpred, i.indrelid) ~
              '^\(state = ANY \(ARRAY\[''succeeded''::(underway\.)?task_state, ''failed''::(underway\.)?task_state\]\)\)$'
          AND i.indexprs IS NULL
    ) THEN
        RAISE EXCEPTION 'idx_task_terminal_created_at is missing, invalid, or has the wrong definition';
    END IF;
END
$$;

COMMENT ON INDEX underway.idx_task_terminal_created_at IS
    'Supports oldest-first retention of terminal Underway tasks without scanning old pending or in_progress rows.';
