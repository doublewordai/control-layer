-- Run before upgrading a populated database, with an explicitly selected schema:
-- psql "$DATABASE_URL" -X -v ON_ERROR_STOP=1 -v schema=public -f scripts/prepare_retained_created_index.sql
-- Do not use --single-transaction: child builds must run concurrently.
-- Rerunnable: valid equivalent children are adopted, even differently named
-- indexes. Invalid/conflicting indexes fail closed and require inspection;
-- this script never drops an index.
\set ON_ERROR_STOP on
\if :{?schema}
\else
    \echo 'Pass -v schema=<target schema>'
    \quit 1
\endif
SELECT format('SET search_path = %I, pg_catalog', :'schema') \gexec
SET lock_timeout = '5s';

-- ONLY creates metadata without building the child indexes.
CREATE INDEX IF NOT EXISTS idx_retained_response_objects_created
    ON ONLY retained_response_objects (created_at DESC, object_id DESC)
    WHERE object_kind = 'request';

CREATE FUNCTION pg_temp.is_retained_created_index(index_oid oid) RETURNS boolean
LANGUAGE sql AS $$
    SELECT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = index_oid AND am.amname = 'btree'
          AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 2 AND i.indnatts = 2
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_at'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'object_id'
          -- pg_get_indexdef omits sort direction; it lives in indoption, where
          -- 3 = DESC NULLS FIRST per key. Checking only the column names would
          -- accept an ASC index, which cannot serve the page ordering.
          AND i.indoption::text = '3 3'
          AND pg_get_expr(i.indpred, i.indrelid) = '(object_kind = ''request''::text)'
    )
$$;
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
        WHERE i.indexrelid = 'idx_retained_response_objects_created'::regclass
          AND i.indrelid = 'retained_response_objects'::regclass
          AND c.relkind = 'I'
          AND pg_temp.is_retained_created_index(i.indexrelid)
    ) THEN
        RAISE EXCEPTION 'Existing retained created parent index has the wrong definition';
    END IF;
END
$$;

SELECT format(
    'CREATE INDEX CONCURRENTLY %I ON %s (created_at DESC, object_id DESC) WHERE object_kind = ''request''',
    'retained_created_' || heap.inhrelid::text, heap.inhrelid::regclass
)
FROM pg_inherits heap
WHERE heap.inhparent = 'retained_response_objects'::regclass
  AND NOT EXISTS (
      SELECT 1 FROM pg_index i WHERE i.indrelid = heap.inhrelid
        AND i.indisvalid AND pg_temp.is_retained_created_index(i.indexrelid)
  )
ORDER BY heap.inhrelid
\gexec

SELECT format('ALTER INDEX idx_retained_response_objects_created ATTACH PARTITION %s', candidate.indexrelid::regclass)
FROM pg_inherits heap
CROSS JOIN LATERAL (
    SELECT i.indexrelid FROM pg_index i
    WHERE i.indrelid = heap.inhrelid AND i.indisvalid
      AND pg_temp.is_retained_created_index(i.indexrelid)
      AND NOT EXISTS (SELECT 1 FROM pg_inherits attached WHERE attached.inhrelid = i.indexrelid)
    ORDER BY i.indexrelid LIMIT 1
) candidate
WHERE heap.inhparent = 'retained_response_objects'::regclass
  AND NOT EXISTS (
      SELECT 1 FROM pg_inherits attached JOIN pg_index i ON i.indexrelid = attached.inhrelid
      WHERE attached.inhparent = 'idx_retained_response_objects_created'::regclass
        AND i.indrelid = heap.inhrelid
  )
ORDER BY heap.inhrelid
\gexec

-- Verify the same definition, validity and attachments as startup. Partition
-- churn can leave an incomplete parent: rerun preparation in that case.
BEGIN;
\ir ../fusillade-arsenal/migrations/20260910000000_add_retained_response_created_index.up.sql
COMMIT;

-- Refresh only this retained store and its children after the new index is valid.
ANALYZE retained_response_objects;
