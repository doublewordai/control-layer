-- Serialize creator-attributed writes against erasure. The creator value is
-- application-defined and should be an opaque, non-reused identifier.
CREATE TABLE creator_erasure_tombstones (
    creator_id TEXT PRIMARY KEY CHECK (BTRIM(creator_id) <> ''),
    erased_at TIMESTAMPTZ NOT NULL
);

-- Soft-deleted parent rows remain for API and ledger integrity. Once erasure
-- finalizes, mark them so a delayed worker cannot repopulate scrubbed metadata.
ALTER TABLE files ADD COLUMN content_erased_at TIMESTAMPTZ;
ALTER TABLE batches ADD COLUMN content_erased_at TIMESTAMPTZ;

CREATE FUNCTION prevent_erased_parent_content_update()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.content_erased_at IS NOT NULL THEN
        RAISE EXCEPTION 'content is permanently erased for this record'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER files_erased_content_update_guard
BEFORE UPDATE OF name, description, error_message, purpose, uploaded_by,
    api_key_id, source_connection_id, source_external_key ON files
FOR EACH ROW EXECUTE FUNCTION prevent_erased_parent_content_update();

CREATE TRIGGER batches_erased_content_update_guard
BEFORE UPDATE OF metadata, errors, created_by, api_key_id, api_key ON batches
FOR EACH ROW EXECUTE FUNCTION prevent_erased_parent_content_update();

COMMENT ON TABLE creator_erasure_tombstones IS
    'Persistent replay guard for creator-scoped erasure; erased creators cannot acquire new content rows.';

-- Inserts acquire a shared transaction advisory lock once per distinct owner,
-- not once per row. This keeps large set-based batch inserts set-based and
-- avoids a hot tombstone update on ordinary writes. Erasure takes the matching
-- exclusive lock before installing the permanent tombstone.
CREATE FUNCTION lock_creator_erasure_shared(current_owner TEXT)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_advisory_xact_lock_shared(
        hashtextextended('creator-erasure:' || current_owner, 0)
    );
    IF EXISTS (
        SELECT 1 FROM creator_erasure_tombstones
        WHERE creator_id = current_owner
    ) THEN
        RAISE EXCEPTION 'persistent data creation is blocked for this creator'
            USING ERRCODE = '23514';
    END IF;
    RETURN;
END;
$$;

CREATE FUNCTION enforce_file_creator_erasure_insert()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE current_owner TEXT;
BEGIN
    FOR current_owner IN
        SELECT DISTINCT BTRIM(uploaded_by)
        FROM new_rows
        WHERE NULLIF(BTRIM(uploaded_by), '') IS NOT NULL
        ORDER BY BTRIM(uploaded_by)
    LOOP
        PERFORM lock_creator_erasure_shared(current_owner);
    END LOOP;
    RETURN NULL;
END;
$$;

CREATE FUNCTION enforce_batch_creator_erasure_insert()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE current_owner TEXT;
BEGIN
    FOR current_owner IN
        SELECT DISTINCT BTRIM(created_by)
        FROM new_rows
        WHERE NULLIF(BTRIM(created_by), '') IS NOT NULL
        ORDER BY BTRIM(created_by)
    LOOP
        PERFORM lock_creator_erasure_shared(current_owner);
    END LOOP;
    RETURN NULL;
END;
$$;

CREATE FUNCTION enforce_request_creator_erasure_insert()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE current_owner TEXT;
BEGIN
    FOR current_owner IN
        SELECT owner FROM (
            SELECT DISTINCT BTRIM(created_by) AS owner
            FROM new_rows
            WHERE NULLIF(BTRIM(created_by), '') IS NOT NULL
            UNION
            SELECT DISTINCT BTRIM(batch.created_by) AS owner
            FROM (
                SELECT DISTINCT batch_id
                FROM new_rows
                WHERE created_by IS NULL AND batch_id IS NOT NULL
            ) request
            JOIN batches batch ON batch.id = request.batch_id
            WHERE NULLIF(BTRIM(batch.created_by), '') IS NOT NULL
        ) owners
        ORDER BY owner
    LOOP
        PERFORM lock_creator_erasure_shared(current_owner);
    END LOOP;
    RETURN NULL;
END;
$$;

-- Attribution changes are exceptional, so a row trigger keeps this path
-- simple without adding overhead to ordinary state/body updates.
CREATE FUNCTION enforce_creator_erasure_update()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    current_owner TEXT;
BEGIN
    current_owner := NULLIF(BTRIM(to_jsonb(NEW) ->> TG_ARGV[0]), '');
    IF current_owner IS NULL AND TG_TABLE_NAME = 'requests' THEN
        SELECT NULLIF(BTRIM(created_by), '')
        INTO current_owner
        FROM batches
        WHERE id = NULLIF(to_jsonb(NEW) ->> 'batch_id', '')::UUID;
    END IF;
    IF current_owner IS NULL THEN
        RETURN NEW;
    END IF;

    PERFORM pg_advisory_xact_lock_shared(
        hashtextextended('creator-erasure:' || current_owner, 0)
    );
    IF EXISTS (
        SELECT 1 FROM creator_erasure_tombstones
        WHERE creator_id = current_owner
    ) THEN
        RAISE EXCEPTION 'persistent data creation is blocked for this creator'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER files_creator_erasure_insert_guard
AFTER INSERT ON files
REFERENCING NEW TABLE AS new_rows
FOR EACH STATEMENT EXECUTE FUNCTION enforce_file_creator_erasure_insert();

CREATE TRIGGER batches_creator_erasure_insert_guard
AFTER INSERT ON batches
REFERENCING NEW TABLE AS new_rows
FOR EACH STATEMENT EXECUTE FUNCTION enforce_batch_creator_erasure_insert();

CREATE TRIGGER requests_creator_erasure_insert_guard
AFTER INSERT ON requests
REFERENCING NEW TABLE AS new_rows
FOR EACH STATEMENT EXECUTE FUNCTION enforce_request_creator_erasure_insert();

CREATE TRIGGER files_creator_erasure_update_guard
BEFORE UPDATE OF uploaded_by ON files
FOR EACH ROW EXECUTE FUNCTION enforce_creator_erasure_update('uploaded_by');

CREATE TRIGGER batches_creator_erasure_update_guard
BEFORE UPDATE OF created_by ON batches
FOR EACH ROW EXECUTE FUNCTION enforce_creator_erasure_update('created_by');

CREATE TRIGGER requests_creator_erasure_update_guard
BEFORE UPDATE OF created_by, batch_id ON requests
FOR EACH ROW EXECUTE FUNCTION enforce_creator_erasure_update('created_by');
