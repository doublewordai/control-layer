-- Additive storage for immutable terminal batchless response graphs.
--
-- This migration does not scan, copy, rename, replace, or rewrite requests,
-- request_templates, or response_steps. Movement and retirement are runtime
-- operations and remain disabled unless an operator enables them separately.
--
-- Before enabling terminal-response movement, build this payload-free index
-- outside a transaction and verify retained_response_archive_index_ready().
-- Its expression supports the bounded terminal/dwell and future-day seed
-- window; complete graph eligibility is revalidated after locking:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_batchless_retention_due
--     ON requests (
--       service_tier,
--       (CASE state WHEN 'completed' THEN completed_at
--                   WHEN 'failed' THEN failed_at
--                   WHEN 'canceled' THEN canceled_at END),
--       id
--     )
--     WHERE batch_id IS NULL
--       AND state IN ('completed', 'failed', 'canceled');
--
-- The existing metadata-retention phases also benefit from these optional
-- operator-built indexes. They are intentionally not built by this migration:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_files_retention_due
--     ON files (expires_at, id)
--     WHERE deleted_at IS NULL AND expires_at IS NOT NULL
--       AND status IN ('processed', 'expired');
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_retention_due
--     ON batches (counts_frozen_at, id)
--     WHERE deleted_at IS NULL AND counts_frozen_at IS NOT NULL;
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_archive_bucket_retirement
--     ON batches (archive_bucket, id)
--     WHERE archive_bucket IS NOT NULL;

SET LOCAL lock_timeout = '5s';

ALTER TABLE batches
    ADD COLUMN retention_expired_at TIMESTAMPTZ;
ALTER TABLE files
    ADD COLUMN retention_expired_at TIMESTAMPTZ;

COMMENT ON COLUMN batches.retention_expired_at IS
    'Set only by scheduled metadata retention after partition-backed payload placement is verified.';
COMMENT ON COLUMN files.retention_expired_at IS
    'Set only by scheduled metadata retention; explicit erasure remains a separate operation.';

CREATE TABLE retained_response_objects (
    delete_on DATE NOT NULL,
    group_id UUID NOT NULL,
    object_kind TEXT NOT NULL CHECK (object_kind IN ('group', 'request', 'step')),
    object_id UUID NOT NULL,
    request_id UUID,
    head_step_id UUID,
    created_by TEXT,
    service_tier TEXT,
    state TEXT,
    model TEXT,
    created_at TIMESTAMPTZ,
    terminal_at TIMESTAMPTZ,
    step_sequence BIGINT,
    schema_version SMALLINT NOT NULL,
    payload JSONB NOT NULL,
    PRIMARY KEY (delete_on, object_kind, object_id)
) PARTITION BY RANGE (delete_on);

COMMENT ON TABLE retained_response_objects IS
    'Daily-partitioned immutable snapshots for complete terminal batchless response graphs.';
COMMENT ON COLUMN retained_response_objects.delete_on IS
    'UTC deletion date. Content expiring on UTC date D is stored under D + 1.';

CREATE INDEX idx_retained_response_objects_group
    ON retained_response_objects
       (delete_on, group_id, object_kind, step_sequence, object_id);
CREATE INDEX idx_retained_response_objects_request_id
    ON retained_response_objects
       (delete_on, request_id, step_sequence, object_id)
    WHERE request_id IS NOT NULL;
CREATE INDEX idx_retained_response_objects_request_object_id
    ON retained_response_objects (delete_on, object_id)
    WHERE object_kind = 'request';
CREATE INDEX idx_retained_response_objects_step_object_id
    ON retained_response_objects (delete_on, object_id)
    WHERE object_kind = 'step';
CREATE INDEX idx_retained_response_objects_owner_created
    ON retained_response_objects
       (created_by, created_at DESC, object_id DESC, delete_on)
    WHERE object_kind = 'request';
CREATE INDEX idx_retained_response_objects_state_created
    ON retained_response_objects
       (state, created_at DESC, delete_on, object_id)
    WHERE object_kind = 'request';
CREATE INDEX idx_retained_response_objects_model_created
    ON retained_response_objects
       (model, created_at DESC, delete_on, object_id)
    WHERE object_kind = 'request';
CREATE INDEX idx_retained_response_objects_tier_created
    ON retained_response_objects
       (service_tier, created_at DESC, delete_on, object_id)
    WHERE object_kind = 'request';

CREATE TABLE retained_response_buckets (
    delete_on DATE PRIMARY KEY,
    partition_schema TEXT NOT NULL,
    partition_table TEXT NOT NULL,
    partition_oid OID NOT NULL,
    state TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'retiring', 'retired')),
    state_changed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (partition_schema, partition_table)
);

COMMENT ON TABLE retained_response_buckets IS
    'Content-free daily-partition identity and read fence for retained responses.';

CREATE TABLE retained_response_group_routes (
    group_id UUID PRIMARY KEY,
    delete_on DATE NOT NULL REFERENCES retained_response_buckets(delete_on)
);

CREATE TABLE retained_response_request_routes (
    request_id UUID PRIMARY KEY,
    group_id UUID NOT NULL,
    delete_on DATE NOT NULL REFERENCES retained_response_buckets(delete_on)
);
CREATE INDEX idx_retained_response_request_routes_group
    ON retained_response_request_routes (group_id, request_id);
CREATE INDEX idx_retained_response_request_routes_bucket
    ON retained_response_request_routes (delete_on, request_id);

CREATE TABLE retained_response_step_routes (
    step_id UUID PRIMARY KEY,
    group_id UUID NOT NULL,
    delete_on DATE NOT NULL REFERENCES retained_response_buckets(delete_on)
);
CREATE INDEX idx_retained_response_step_routes_group
    ON retained_response_step_routes (group_id, step_id);
CREATE INDEX idx_retained_response_step_routes_bucket
    ON retained_response_step_routes (delete_on, step_id);

COMMENT ON TABLE retained_response_group_routes IS
    'Content-free exact group-to-day routing metadata.';
COMMENT ON TABLE retained_response_request_routes IS
    'Content-free exact request-to-group-and-day routing metadata.';
COMMENT ON TABLE retained_response_step_routes IS
    'Content-free exact response-step-to-group-and-day routing metadata.';

CREATE TABLE retained_response_resurrection_fences (
    object_id UUID PRIMARY KEY,
    reason TEXT NOT NULL CHECK (reason IN ('archived', 'erased', 'retired')),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX idx_retained_response_resurrection_fences_expiry
    ON retained_response_resurrection_fences (expires_at, object_id);

COMMENT ON TABLE retained_response_resurrection_fences IS
    'Content-free bounded UUID fences preventing destructive response lifecycle events from being undone by late writers.';

-- One OID-fenced journal and transaction-pool-safe lease is shared by weekly
-- batch partitions and daily retained-response partitions.
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
    'Crash-recovery journal and transaction-pool-safe lease for OID-fenced partition retirement.';

CREATE FUNCTION retained_response_archive_index_ready(
    relation_schema TEXT DEFAULT NULL
)
RETURNS BOOLEAN
LANGUAGE sql
STABLE
SET search_path FROM CURRENT
AS $$
    SELECT EXISTS (
        SELECT 1
        FROM pg_index index_catalog
        JOIN pg_class heap ON heap.oid = index_catalog.indrelid
        JOIN pg_class index_relation ON index_relation.oid = index_catalog.indexrelid
        JOIN pg_namespace namespace ON namespace.oid = heap.relnamespace
        JOIN pg_am access_method ON access_method.oid = index_relation.relam
        WHERE namespace.nspname = COALESCE(relation_schema, current_schema())
          AND heap.relname = 'requests'
          AND index_relation.relname = 'idx_requests_batchless_retention_due'
          AND index_catalog.indisready
          AND index_catalog.indisvalid
          AND access_method.amname = 'btree'
          AND index_catalog.indnkeyatts = 3
          AND index_catalog.indnatts = 3
          AND pg_get_indexdef(index_catalog.indexrelid, 1, TRUE) = 'service_tier'
          AND lower(regexp_replace(
                  regexp_replace(
                      pg_get_indexdef(index_catalog.indexrelid, 2, TRUE),
                      '::(text|timestamp with time zone)', '', 'g'
                  ),
                  '[[:space:]()]', '', 'g'
              )) = 'casestatewhen''completed''thencompleted_atwhen''failed''thenfailed_atwhen''canceled''thencanceled_atelsenullend'
          AND pg_get_indexdef(index_catalog.indexrelid, 3, TRUE) = 'id'
          AND lower(regexp_replace(
                  regexp_replace(
                      pg_get_expr(index_catalog.indpred, index_catalog.indrelid, TRUE),
                      '::text', '', 'g'
                  ),
                  '[[:space:]()]', '', 'g'
              )) = 'batch_idisnullandstate=anyarray[''completed'',''failed'',''canceled'']'
    )
$$;

COMMENT ON FUNCTION retained_response_archive_index_ready(TEXT) IS
    'True only when the operator-built batchless terminal candidate index has the canonical keys and predicate and is ready and valid.';

CREATE FUNCTION ensure_retained_response_partition(
    target_delete_on DATE,
    relation_schema TEXT DEFAULT NULL
)
RETURNS VOID
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
DECLARE
    schema_name TEXT := COALESCE(relation_schema, current_schema());
    partition_name TEXT;
    partition_end DATE;
    parent_oid OID;
    child_oid OID;
    attached_with_exact_bounds BOOLEAN;
    bucket_matches BOOLEAN;
BEGIN
    IF schema_name IS DISTINCT FROM current_schema() THEN
        RAISE EXCEPTION USING
            ERRCODE = 'invalid_schema_name',
            MESSAGE = 'relation_schema must match the retained-response helper schema';
    END IF;

    IF target_delete_on IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = 'null_value_not_allowed',
            MESSAGE = 'target_delete_on must not be null';
    END IF;

    partition_name := 'retained_response_objects_d'
        || to_char(target_delete_on, 'YYYYMMDD');
    partition_end := target_delete_on + 1;
    parent_oid := to_regclass(format('%I.retained_response_objects', schema_name));
    IF parent_oid IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = 'undefined_table',
            MESSAGE = 'retained response parent is missing';
    END IF;

    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            'retained_response_objects.partition:' || schema_name || ':'
                || target_delete_on::text,
            0
        )
    );

    child_oid := to_regclass(format('%I.%I', schema_name, partition_name));
    IF child_oid IS NULL THEN
        IF EXISTS (
            SELECT 1
            FROM retained_response_buckets bucket
            WHERE bucket.delete_on = target_delete_on
        ) THEN
            RAISE EXCEPTION USING
                ERRCODE = 'object_not_in_prerequisite_state',
                MESSAGE = 'retained response bucket identity exists without its partition';
        END IF;

        EXECUTE format(
            'CREATE TABLE %I.%I (LIKE %I.retained_response_objects INCLUDING ALL)',
            schema_name,
            partition_name,
            schema_name
        );
        EXECUTE format(
            'ALTER TABLE %I.%I ADD CONSTRAINT %I CHECK '
                || '(delete_on >= %L::date AND delete_on < %L::date)',
            schema_name,
            partition_name,
            partition_name || '_delete_on_bounds',
            target_delete_on,
            partition_end
        );
        EXECUTE format(
            'ALTER TABLE %I.retained_response_objects ATTACH PARTITION %I.%I '
                || 'FOR VALUES FROM (%L) TO (%L)',
            schema_name,
            schema_name,
            partition_name,
            target_delete_on,
            partition_end
        );
        child_oid := to_regclass(format('%I.%I', schema_name, partition_name));

        INSERT INTO retained_response_buckets (
            delete_on,
            partition_schema,
            partition_table,
            partition_oid
        ) VALUES (
            target_delete_on,
            schema_name,
            partition_name,
            child_oid
        );
    END IF;

    SELECT EXISTS (
        SELECT 1
        FROM pg_inherits inheritance
        JOIN pg_class child ON child.oid = inheritance.inhrelid
        WHERE inheritance.inhparent = parent_oid
          AND inheritance.inhrelid = child_oid
          AND NOT inheritance.inhdetachpending
          AND pg_get_expr(child.relpartbound, child.oid) = format(
              'FOR VALUES FROM (%L) TO (%L)',
              target_delete_on,
              partition_end
          )
    ) INTO attached_with_exact_bounds;
    IF NOT attached_with_exact_bounds THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'retained response partition has unexpected attachment or bounds';
    END IF;

    INSERT INTO retained_response_buckets (
        delete_on,
        partition_schema,
        partition_table,
        partition_oid
    ) VALUES (
        target_delete_on,
        schema_name,
        partition_name,
        child_oid
    )
    ON CONFLICT (delete_on) DO NOTHING;

    SELECT EXISTS (
        SELECT 1
        FROM retained_response_buckets bucket
        WHERE bucket.delete_on = target_delete_on
          AND bucket.partition_schema = schema_name
          AND bucket.partition_table = partition_name
          AND bucket.partition_oid = child_oid
          AND bucket.state = 'active'
    ) INTO bucket_matches;
    IF NOT bucket_matches THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'retained response bucket identity is fenced or inconsistent';
    END IF;
END;
$$;

COMMENT ON FUNCTION ensure_retained_response_partition(DATE, TEXT) IS
    'Idempotently creates and records one exact daily UTC delete_on partition in the helper-owning schema using a transaction advisory lock.';

CREATE FUNCTION ensure_retained_response_partitions(
    first_delete_on DATE,
    days_ahead INTEGER
)
RETURNS INTEGER
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
DECLARE
    target_delete_on DATE;
    partition_name TEXT;
    created INTEGER := 0;
BEGIN
    IF first_delete_on IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = 'null_value_not_allowed',
            MESSAGE = 'first_delete_on must not be null';
    END IF;
    IF days_ahead < 0 THEN
        RAISE EXCEPTION USING
            ERRCODE = 'invalid_parameter_value',
            MESSAGE = 'days_ahead must not be negative';
    END IF;

    FOR offset_days IN 0..days_ahead LOOP
        target_delete_on := first_delete_on + offset_days;
        partition_name := 'retained_response_objects_d'
            || to_char(target_delete_on, 'YYYYMMDD');
        PERFORM pg_advisory_xact_lock(
            hashtextextended(
                'retained_response_objects.partition:' || current_schema() || ':'
                    || target_delete_on::text,
                0
            )
        );
        IF to_regclass(format('%I.%I', current_schema(), partition_name)) IS NULL THEN
            created := created + 1;
        END IF;
        PERFORM ensure_retained_response_partition(target_delete_on, current_schema());
    END LOOP;
    RETURN created;
END;
$$;

COMMENT ON FUNCTION ensure_retained_response_partitions(DATE, INTEGER) IS
    'Concurrently ensures and exactly counts daily partitions from first_delete_on through days_ahead, inclusive.';
