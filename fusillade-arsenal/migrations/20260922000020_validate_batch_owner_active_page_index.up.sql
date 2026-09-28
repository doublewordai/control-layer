-- Validate the index created by 20260922000000 and reindexed by
-- 20260922000010 (docs/migrations.md, "Concurrent index builds").
--
-- IF NOT EXISTS in the create migration is never proof the index is usable:
-- an interrupted build is recorded as applied while leaving an INVALID index.
-- This file fails for anything other than the exact definition the query in
-- fusillade-arsenal/src/postgres/batch_list.rs plans against, so a bad build
-- blocks the rollout instead of silently leaving owner pages to walk history.
-- A failure here is an operator decision (repair or drop by hand), never
-- something a migration guesses at.

DO $$
DECLARE
    page_index regclass := to_regclass(format('%I.idx_batches_owner_active_created_at_id', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = page_index
          AND i.indrelid = to_regclass(format('%I.batches', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 3 AND i.indnatts = 3
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_by'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'created_at'
          AND pg_get_indexdef(i.indexrelid, 3, true) = 'id'
          AND i.indoption::text = '0 3 3'
          AND pg_get_expr(i.indpred, i.indrelid) = '(deleted_at IS NULL) AND (completed_at IS NULL) AND (failed_at IS NULL) AND (cancelled_at IS NULL) AND (cancelling_at IS NULL)'
    ) THEN
        RAISE EXCEPTION 'idx_batches_owner_active_created_at_id is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_batches_owner_active_created_at_id IS
'Owner-scoped active-first batch pages. The list arm filters exactly this predicate set, so the index serves the page order (created_at DESC, id DESC) over the owner''s live rows only, without walking the owner''s terminal history. Holds only active batches, so it tracks the working set rather than history. Pinned predicate must move with the ACTIVE constant in batch_list.rs.';