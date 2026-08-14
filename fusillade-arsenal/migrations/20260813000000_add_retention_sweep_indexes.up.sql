-- Policy-driven content expiration uses two storage shapes:
--
-- * terminal batch request payloads already live in weekly archive partitions;
-- * new request templates are routed into weekly partitions without copying the
--   existing template table during deployment.
--
-- The legacy template table is renamed in place and exposed together with the
-- partitioned store through a compatibility view. This is a metadata-only
-- cutover for existing rows; no heap or TOAST data is rewritten.
--
-- Large installations should create the four candidate indexes concurrently
-- before deploying this migration, using the component schema search_path:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_files_retention_due
--     ON files (expires_at, id)
--     WHERE deleted_at IS NULL AND expires_at IS NOT NULL
--       AND status IN ('processed', 'expired');
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_retention_due
--     ON batches (counts_frozen_at, id)
--     WHERE deleted_at IS NULL AND counts_frozen_at IS NOT NULL;
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_batchless_retention_due
--     ON requests (service_tier,
--       (CASE state WHEN 'completed' THEN completed_at
--                   WHEN 'failed' THEN failed_at
--                   WHEN 'canceled' THEN canceled_at END), id)
--     WHERE batch_id IS NULL
--       AND state IN ('completed', 'failed', 'canceled');
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_archive_bucket_retirement
--     ON batches (archive_bucket, id)
--     WHERE archive_bucket IS NOT NULL;
--
-- Verify each prebuilt index reports indisready=true and indisvalid=true in
-- pg_index. The transactional IF NOT EXISTS statements below should then be
-- metadata-only no-ops.

SET LOCAL lock_timeout = '5s';

ALTER TABLE batches
    ADD COLUMN retention_expired_at TIMESTAMPTZ;
ALTER TABLE files
    ADD COLUMN retention_expired_at TIMESTAMPTZ;

COMMENT ON COLUMN batches.retention_expired_at IS
    'Set only by scheduled retention. Explicit deletion leaves this NULL so '
    'the orphan purger can distinguish immediate erasure from partition '
    'retirement.';
COMMENT ON COLUMN files.retention_expired_at IS
    'Set only by scheduled retention so explicit erasure remains an immediate '
    'row-targeted operation while scheduled payloads retire by partition.';

CREATE INDEX IF NOT EXISTS idx_files_retention_due
ON files (expires_at, id)
WHERE deleted_at IS NULL
  AND expires_at IS NOT NULL
  AND status IN ('processed', 'expired');

CREATE INDEX IF NOT EXISTS idx_batches_retention_due
ON batches (counts_frozen_at, id)
WHERE deleted_at IS NULL
  AND counts_frozen_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_batchless_retention_due
ON requests (
    service_tier,
    (CASE state
       WHEN 'completed' THEN completed_at
       WHEN 'failed' THEN failed_at
       WHEN 'canceled' THEN canceled_at
     END),
    id
)
WHERE batch_id IS NULL
  AND state IN ('completed', 'failed', 'canceled');

CREATE INDEX IF NOT EXISTS idx_batches_archive_bucket_retirement
ON batches (archive_bucket, id)
WHERE archive_bucket IS NOT NULL;

ALTER TABLE request_templates RENAME TO request_templates_legacy;
ALTER TABLE request_templates_legacy
    ADD COLUMN retained_bucket TIMESTAMPTZ;

COMMENT ON COLUMN request_templates_legacy.retained_bucket IS
    'NULL identifies a pre-cutover payload row. Non-NULL rows are narrow ID '
    'and routing stubs whose payload is stored in request_templates_retained. '
    'Keeping stubs here preserves the existing requests foreign key without '
    'backfilling legacy IDs.';

CREATE TABLE request_templates_retained (
    id UUID NOT NULL DEFAULT gen_random_uuid(),
    file_id UUID REFERENCES files(id) ON DELETE SET NULL,
    endpoint TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL,
    api_key TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    custom_id TEXT,
    line_number INTEGER NOT NULL DEFAULT 0,
    body_byte_size BIGINT NOT NULL DEFAULT 0,
    metadata JSONB,
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

CREATE INDEX idx_request_templates_retained_id
    ON request_templates_retained (id);
CREATE INDEX idx_request_templates_retained_file_id
    ON request_templates_retained (file_id);
CREATE INDEX idx_request_templates_retained_custom_id
    ON request_templates_retained (custom_id);
CREATE INDEX idx_request_templates_retained_file_line
    ON request_templates_retained (file_id, line_number);
CREATE INDEX idx_request_templates_retained_model
    ON request_templates_retained (model);
CREATE INDEX idx_request_templates_retained_file_model
    ON request_templates_retained (file_id, model);

-- New-only locator rows keep partition routing and retirement checks bounded.
-- Legacy IDs are deliberately not backfilled.
CREATE TABLE request_template_retained_keys (
    id UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL,
    file_id UUID,
    line_number INTEGER NOT NULL,
    scrubbed_at TIMESTAMPTZ,
    UNIQUE (id, created_at)
);
CREATE INDEX idx_request_template_retained_keys_created_at
    ON request_template_retained_keys (created_at, id);
CREATE INDEX idx_request_template_retained_keys_file_line
    ON request_template_retained_keys (file_id, line_number);

-- A file normally occupies one weekly partition (two only when an upload
-- crosses the UTC week boundary). This narrow route makes file-oriented reads
-- perform one range scan per owning bucket instead of one UUID probe per row.
CREATE TABLE request_template_retained_file_buckets (
    file_id UUID NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    bucket_start TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (file_id, bucket_start)
);

CREATE TABLE retention_partition_retirements (
    parent_table TEXT NOT NULL,
    partition_table TEXT NOT NULL,
    partition_oid OID NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    lease_owner UUID,
    lease_expires_at TIMESTAMPTZ,
    PRIMARY KEY (parent_table, partition_table)
);

COMMENT ON TABLE retention_partition_retirements IS
    'Crash-recovery journal and transaction-pool-safe lease for concurrent partition detach followed by drop.';

CREATE TABLE request_template_storage_cutover (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    cutover_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
INSERT INTO request_template_storage_cutover DEFAULT VALUES;

CREATE FUNCTION request_template_legacy_retirement_ready(minimum_age INTERVAL)
RETURNS BOOLEAN
LANGUAGE sql
STABLE
SET search_path FROM CURRENT
AS $$
    SELECT NOW() >= cutover_at + minimum_age
       AND NOT EXISTS (
           SELECT 1
           FROM request_templates_legacy legacy
           LEFT JOIN files file ON file.id = legacy.file_id
           WHERE legacy.retained_bucket IS NULL
             AND (
                 (legacy.file_id IS NOT NULL AND file.deleted_at IS NULL)
                 OR (legacy.file_id IS NOT NULL AND EXISTS (
                     SELECT 1 FROM batches batch
                     WHERE batch.file_id = legacy.file_id
                       AND batch.deleted_at IS NULL
                 ))
                 OR EXISTS (
                     SELECT 1 FROM requests request
                     WHERE request.template_id = legacy.id
                 )
             )
       )
    FROM request_template_storage_cutover
    WHERE singleton
$$;

COMMENT ON FUNCTION request_template_legacy_retirement_ready(INTERVAL) IS
    'Readiness gate for the one-time follow-up migration that removes the '
    'pre-cutover payload generation. Call with the deployment retention age; '
    'the check never reads body/TOAST data.';

CREATE FUNCTION ensure_request_template_partition(
    target_at TIMESTAMPTZ,
    relation_schema TEXT DEFAULT NULL
)
RETURNS VOID
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
DECLARE
    schema_name TEXT := COALESCE(relation_schema, current_schema());
    bucket_start TIMESTAMPTZ := date_trunc('week', target_at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
    bucket_end TIMESTAMPTZ := bucket_start + INTERVAL '7 days';
    partition_name TEXT := 'request_templates_retained_y'
        || to_char(bucket_start AT TIME ZONE 'UTC', 'IYYY')
        || 'w' || to_char(bucket_start AT TIME ZONE 'UTC', 'IW');
BEGIN
    IF to_regclass(format('%I.%I', schema_name, partition_name)) IS NOT NULL THEN
        RETURN;
    END IF;

    PERFORM pg_advisory_xact_lock(
        hashtextextended('request_templates_retained.partition:' || schema_name, 0)
    );
    IF to_regclass(format('%I.%I', schema_name, partition_name)) IS NULL THEN
        EXECUTE format(
            'CREATE TABLE %I.%I (LIKE %I.request_templates_retained INCLUDING ALL)',
            schema_name, partition_name, schema_name
        );
        EXECUTE format(
            'ALTER TABLE %I.%I ADD CONSTRAINT %I CHECK '
            || '(created_at >= %L::timestamptz AND created_at < %L::timestamptz)',
            schema_name, partition_name,
            partition_name || '_bounds',
            bucket_start,
            bucket_end
        );
        EXECUTE format(
            'ALTER TABLE %I.request_templates_retained ATTACH PARTITION %I.%I '
            || 'FOR VALUES FROM (%L) TO (%L)',
            schema_name, schema_name, partition_name,
            bucket_start,
            bucket_end
        );
        EXECUTE format(
            'ALTER TABLE %I.%I DROP CONSTRAINT %I',
            schema_name, partition_name,
            partition_name || '_bounds'
        );
    END IF;
END;
$$;

CREATE FUNCTION ensure_request_template_partitions(weeks_ahead INTEGER DEFAULT 4)
RETURNS INTEGER
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
DECLARE
    schema_name TEXT := current_schema();
    target TIMESTAMPTZ;
    partition_name TEXT;
    created INTEGER := 0;
BEGIN
    FOR i IN 0..GREATEST(weeks_ahead, 0) LOOP
        target := date_trunc('week', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
            + (i * INTERVAL '7 days');
        partition_name := 'request_templates_retained_y'
            || to_char(target AT TIME ZONE 'UTC', 'IYYY')
            || 'w' || to_char(target AT TIME ZONE 'UTC', 'IW');
        IF to_regclass(format('%I.%I', schema_name, partition_name)) IS NULL THEN
            PERFORM ensure_request_template_partition(target, schema_name);
            created := created + 1;
        END IF;
    END LOOP;
    RETURN created;
END;
$$;

SELECT ensure_request_template_partitions(4);

-- The compatibility relation retains the exact public column order and shape
-- used by existing queries.
CREATE VIEW request_templates AS
SELECT id, file_id, endpoint, method, path, body, model, api_key,
       created_at, updated_at, custom_id, line_number, body_byte_size, metadata
FROM request_templates_legacy
WHERE retained_bucket IS NULL
UNION ALL
SELECT registry.id, registry.file_id, payload.endpoint, payload.method,
       payload.path, payload.body, payload.model, payload.api_key,
       payload.created_at, payload.updated_at, payload.custom_id,
       registry.line_number, payload.body_byte_size, payload.metadata
FROM request_template_retained_keys registry
CROSS JOIN LATERAL (
    SELECT stored.*
    FROM request_templates_retained stored
    WHERE stored.id = registry.id
      AND stored.created_at = registry.created_at
    -- Preserve the parameterized inner scan so execution-time partition
    -- pruning uses the locator timestamp instead of probing every child.
    OFFSET 0
) payload;

-- Set-oriented file reads use the file-to-bucket route. The LATERAL boundary
-- preserves execution-time partition pruning while the inner scan uses the
-- child's (file_id, line_number) index once per bucket.
CREATE VIEW request_templates_by_file AS
SELECT id, file_id, endpoint, method, path, body, model, api_key,
       created_at, updated_at, custom_id, line_number, body_byte_size, metadata
FROM request_templates_legacy
WHERE retained_bucket IS NULL
UNION ALL
SELECT payload.id, route.file_id, payload.endpoint, payload.method,
       payload.path, payload.body, payload.model, payload.api_key,
       payload.created_at, payload.updated_at, payload.custom_id,
       payload.line_number, payload.body_byte_size, payload.metadata
FROM request_template_retained_file_buckets route
CROSS JOIN LATERAL (
    SELECT stored.*
    FROM request_templates_retained stored
    WHERE stored.file_id = route.file_id
      AND stored.created_at >= route.bucket_start
      AND stored.created_at < route.bucket_start + INTERVAL '7 days'
    OFFSET 0
) payload;

ALTER VIEW request_templates ALTER COLUMN id SET DEFAULT gen_random_uuid();
ALTER VIEW request_templates ALTER COLUMN body SET DEFAULT '';
ALTER VIEW request_templates ALTER COLUMN created_at SET DEFAULT NOW();
ALTER VIEW request_templates ALTER COLUMN updated_at SET DEFAULT NOW();
ALTER VIEW request_templates ALTER COLUMN line_number SET DEFAULT 0;
ALTER VIEW request_templates ALTER COLUMN body_byte_size SET DEFAULT 0;

CREATE FUNCTION route_request_template_write()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
DECLARE
    affected INTEGER;
BEGIN
    IF TG_OP = 'INSERT' THEN
        NEW.id := COALESCE(NEW.id, gen_random_uuid());
        NEW.body := COALESCE(NEW.body, '');
        NEW.created_at := COALESCE(NEW.created_at, NOW());
        NEW.updated_at := COALESCE(NEW.updated_at, NOW());
        NEW.line_number := COALESCE(NEW.line_number, 0);
        NEW.body_byte_size := COALESCE(NEW.body_byte_size, 0);

        -- Trigger execution can occur on a connection whose first search-path
        -- entry differs from the schema containing this compatibility view.
        -- Route by the trigger relation's actual schema, not current_schema().
        PERFORM ensure_request_template_partition(NEW.created_at, TG_TABLE_SCHEMA);
        -- The stub is deliberately content-free. Its primary key supplies
        -- global UUID uniqueness and remains the target of the existing
        -- requests.template_id FK.
        INSERT INTO request_templates_legacy (
            id, file_id, endpoint, method, path, body, model, api_key,
            created_at, updated_at, custom_id, line_number, body_byte_size,
            metadata, retained_bucket
        ) VALUES (
            NEW.id, NEW.file_id, '', '', '', '', '', '',
            NEW.created_at, NEW.updated_at, NULL, NEW.line_number, 0,
            NULL, NEW.created_at
        );
        INSERT INTO request_template_retained_keys (id, created_at, file_id, line_number)
        VALUES (NEW.id, NEW.created_at, NEW.file_id, NEW.line_number);
        IF NEW.file_id IS NOT NULL THEN
            INSERT INTO request_template_retained_file_buckets (file_id, bucket_start)
            VALUES (
                NEW.file_id,
                date_trunc('week', NEW.created_at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
            )
            ON CONFLICT DO NOTHING;
        END IF;
        INSERT INTO request_templates_retained (
            id, file_id, endpoint, method, path, body, model, api_key,
            created_at, updated_at, custom_id, line_number, body_byte_size, metadata
        ) VALUES (
            NEW.id, NEW.file_id, NEW.endpoint, NEW.method, NEW.path, NEW.body,
            NEW.model, NEW.api_key, NEW.created_at, NEW.updated_at, NEW.custom_id,
            NEW.line_number, NEW.body_byte_size, NEW.metadata
        );
        RETURN NEW;
    ELSIF TG_OP = 'UPDATE' THEN
        IF NEW.id <> OLD.id OR NEW.created_at <> OLD.created_at THEN
            RAISE integrity_constraint_violation USING MESSAGE =
                'request template identity and partition timestamp are immutable';
        END IF;
        NEW.updated_at := NOW();
        UPDATE request_templates_retained SET
            file_id = NEW.file_id,
            endpoint = NEW.endpoint,
            method = NEW.method,
            path = NEW.path,
            body = NEW.body,
            model = NEW.model,
            api_key = NEW.api_key,
            updated_at = NEW.updated_at,
            custom_id = NEW.custom_id,
            line_number = NEW.line_number,
            body_byte_size = NEW.body_byte_size,
            metadata = NEW.metadata
        WHERE id = OLD.id AND created_at = OLD.created_at;
        GET DIAGNOSTICS affected = ROW_COUNT;
        IF affected > 0 THEN
            UPDATE request_templates_legacy
            SET file_id = NEW.file_id,
                line_number = NEW.line_number,
                updated_at = NEW.updated_at
            WHERE id = OLD.id AND retained_bucket IS NOT NULL;
            UPDATE request_template_retained_keys
            SET file_id = NEW.file_id,
                line_number = NEW.line_number
            WHERE id = OLD.id AND created_at = OLD.created_at;
            IF NEW.file_id IS NOT NULL THEN
                INSERT INTO request_template_retained_file_buckets (file_id, bucket_start)
                VALUES (
                    NEW.file_id,
                    date_trunc('week', NEW.created_at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                )
                ON CONFLICT DO NOTHING;
            END IF;
        ELSE
            UPDATE request_templates_legacy SET
                file_id = NEW.file_id,
                endpoint = NEW.endpoint,
                method = NEW.method,
                path = NEW.path,
                body = NEW.body,
                model = NEW.model,
                api_key = NEW.api_key,
                custom_id = NEW.custom_id,
                line_number = NEW.line_number,
                body_byte_size = NEW.body_byte_size,
                metadata = NEW.metadata
            WHERE id = OLD.id;
        END IF;
        RETURN NEW;
    ELSE
        DELETE FROM request_templates_retained
        WHERE id = OLD.id AND created_at = OLD.created_at;
        GET DIAGNOSTICS affected = ROW_COUNT;
        IF affected > 0 THEN
            -- Deleting the routing stub invokes the existing FK's ON DELETE
            -- SET NULL action for live requests.
            DELETE FROM request_templates_legacy
            WHERE id = OLD.id AND retained_bucket IS NOT NULL;
            DELETE FROM request_template_retained_keys
            WHERE id = OLD.id AND created_at = OLD.created_at;
        ELSE
            DELETE FROM request_templates_legacy
            WHERE id = OLD.id AND retained_bucket IS NULL;
        END IF;
        RETURN OLD;
    END IF;
END;
$$;

CREATE TRIGGER route_request_template_writes
INSTEAD OF INSERT OR UPDATE OR DELETE ON request_templates
FOR EACH ROW EXECUTE FUNCTION route_request_template_write();

CREATE OR REPLACE VIEW active_request_templates AS
SELECT rt.*
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL;

COMMENT ON TABLE request_templates_legacy IS
    'Pre-cutover payload rows plus content-free routing stubs for partitioned '
    'templates. New payload writes route to request_templates_retained.';

COMMENT ON TABLE request_templates_retained IS
    'Weekly-partitioned request templates created after the zero-copy cutover.';
