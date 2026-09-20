DO $$
DECLARE
    page_index regclass := to_regclass(format('%I.idx_requests_batchless_terminal_model_page', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = page_index
          AND i.indrelid = to_regclass(format('%I.requests', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 4 AND i.indnatts = 4
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'model'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'state'
          AND pg_get_indexdef(i.indexrelid, 3, true) = 'created_at'
          AND pg_get_indexdef(i.indexrelid, 4, true) = 'id'
          AND i.indoption::text = '0 0 3 3'
          AND pg_get_expr(i.indpred, i.indrelid) =
              '((created_by IS NOT NULL) AND (state <> ALL (ARRAY[''processing''::text, ''claimed''::text, ''pending''::text])))'
    ) THEN
        RAISE EXCEPTION 'idx_requests_batchless_terminal_model_page is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_requests_batchless_terminal_model_page IS
'Zero-match status+model Responses pages: (model, state, created_at DESC, id DESC) over batchless terminal rows. Without it the only ordered plan walks the whole terminal history in created_at order with both filters as residual predicates and hits the 30s page budget when the filter matches nothing. The partial predicate matches the terminal arm''s literal state NOT IN qual so the index stays eligible under generic prepared plans.';
