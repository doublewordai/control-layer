-- Index terminal_at on the retained-response store.
--
-- Trailing demand filters retained requests by state and terminal_at.
-- Partition pruning on delete_on limits the daily partitions read, while
-- (state, terminal_at) limits each admitted partition to the matching window.
-- Existing indexes lead on created_at and cannot provide that range scan.
--
-- Populated installations must first run scripts/prepare_retained_terminal_index.sql
-- to build concurrent child indexes and attach them. Startup validates that
-- preparation is complete instead of silently accepting a same-name index or
-- building every populated partition while blocking writes. Fresh empty
-- databases can build immediately. Future partitions inherit the parent index.
SET LOCAL lock_timeout = '5s';

DO $$
DECLARE
    parent_table regclass := to_regclass(format('%I.retained_response_objects', current_schema()));
    parent_index regclass := to_regclass(format('%I.idx_retained_response_objects_state_terminal', current_schema()));
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
            RAISE EXCEPTION 'Prepare the retained terminal index with scripts/prepare_retained_terminal_index.sql before upgrading a populated database';
        END IF;
        EXECUTE format('CREATE INDEX idx_retained_response_objects_state_terminal ON %s (state, terminal_at) WHERE object_kind = ''request''', parent_table);
        parent_index := to_regclass(format('%I.idx_retained_response_objects_state_terminal', current_schema()));
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = parent_index AND i.indrelid = parent_table
          AND c.relkind = 'I' AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 2 AND i.indnatts = 2
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'state'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'terminal_at'
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
        RAISE EXCEPTION 'Retained terminal index is invalid, incomplete, or has the wrong definition; run scripts/prepare_retained_terminal_index.sql before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_retained_response_objects_state_terminal IS
'Trailing terminal-demand window scans: (state, terminal_at) on retained request objects. Existing indexes lead on created_at; without this the demand query filters every row of each admitted daily partition.';
