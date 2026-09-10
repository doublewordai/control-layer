-- Index plain newest-first ordering on the retained-response store.
--
-- The Responses page orders the archive arm by (created_at DESC, object_id
-- DESC). Every existing sort index on retained_response_objects leads with
-- something else -- created_by, state, model or service_tier -- so an unscoped
-- listing (a platform manager with no active organization) has no index that
-- can produce that ordering. The planner falls back to the owner-leading index
-- per partition and sorts the whole retained history, which means the page
-- LIMIT cannot be pushed down.
--
-- Measured on a copy of production: the unscoped page plans at cost 1.19e9 as
-- Append + Sort and takes 124.6s, which is longer than the browser waits.
-- With this index the arm becomes a Merge Append of per-partition index scans
-- and the same page returns in 2.2ms warm. Owner-scoped listings are
-- unaffected (3.2ms before and after).
--
-- Naming the terminal states in the query instead does not work: the planner
-- then prefers idx_retained_response_objects_state_terminal and returns to
-- Append + Sort.
--
-- Populated installations must first run
-- scripts/prepare_retained_created_index.sql to build concurrent child indexes
-- and attach them. Startup validates that preparation is complete instead of
-- silently accepting a same-name index or building every populated partition
-- while blocking writes. Fresh empty databases can build immediately. Future
-- partitions inherit the parent index.
SET LOCAL lock_timeout = '5s';

DO $$
DECLARE
    parent_table regclass := to_regclass(format('%I.retained_response_objects', current_schema()));
    parent_index regclass := to_regclass(format('%I.idx_retained_response_objects_created', current_schema()));
    populated boolean;
BEGIN
    IF parent_index IS NULL THEN
        EXECUTE format('LOCK TABLE %s IN SHARE MODE', parent_table);
        -- Conservatively require preparation even for emptied heaps with dead
        -- pages. Never scan old partitions while holding the SHARE lock.
        SELECT EXISTS (
            SELECT 1 FROM pg_partition_tree(parent_table) tree
            WHERE tree.isleaf AND pg_relation_size(tree.relid) > 0
        ) INTO populated;
        IF populated THEN
            RAISE EXCEPTION 'Prepare the retained created index with scripts/prepare_retained_created_index.sql before upgrading a populated database';
        END IF;
        EXECUTE format('CREATE INDEX idx_retained_response_objects_created ON %s (created_at DESC, object_id DESC) WHERE object_kind = ''request''', parent_table);
        parent_index := to_regclass(format('%I.idx_retained_response_objects_created', current_schema()));
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = parent_index AND i.indrelid = parent_table
          AND c.relkind = 'I' AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 2 AND i.indnatts = 2
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_at'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'object_id'
          -- pg_get_indexdef omits sort direction; it lives in indoption, where
          -- 3 = DESC NULLS FIRST per key. Checking only the column names would
          -- accept an ASC index, which cannot serve the page ordering.
          AND i.indoption::text = '3 3'
          AND pg_get_expr(i.indpred, i.indrelid) = '(object_kind = ''request''::text)'
    ) OR EXISTS (
        SELECT 1 FROM pg_inherits heap
        WHERE heap.inhparent = parent_table
          AND NOT EXISTS (
              SELECT 1 FROM pg_inherits attachment
              JOIN pg_index child ON child.indexrelid = attachment.inhrelid
              WHERE attachment.inhparent = parent_index
                AND child.indrelid = heap.inhrelid
                AND child.indisvalid AND child.indisready
          )
    ) THEN
        RAISE EXCEPTION 'Retained created index is invalid, incomplete, or has the wrong definition; run scripts/prepare_retained_created_index.sql before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_retained_response_objects_created IS
'Unscoped Responses page ordering: (created_at DESC, object_id DESC) on retained request objects. Every other sort index leads with created_by, state, model or service_tier, so without this an unscoped page sorts the whole retained history instead of Merge Appending bounded per-partition scans.';
