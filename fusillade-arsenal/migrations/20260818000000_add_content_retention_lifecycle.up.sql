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
CREATE INDEX idx_retained_response_group_routes_bucket_group
    ON retained_response_group_routes (delete_on, group_id);

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
    partition_schema TEXT,
    partition_schema_oid OID,
    parent_oid OID,
    lower_bound DATE,
    upper_bound DATE,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    lease_owner UUID,
    lease_expires_at TIMESTAMPTZ,
    PRIMARY KEY (parent_table, partition_table),
    CONSTRAINT retained_response_retirement_identity_complete CHECK (
        (
            parent_table <> 'retained_response_objects'
            OR (
                partition_schema IS NOT NULL
                AND partition_schema_oid IS NOT NULL
                AND parent_oid IS NOT NULL
                AND lower_bound IS NOT NULL
                AND upper_bound IS NOT NULL
                AND upper_bound = lower_bound + 1
                AND partition_table =
                    'retained_response_objects_d' || to_char(lower_bound, 'YYYYMMDD')
            )
        ) AND (
            parent_table <> 'batch_requests_archive'
            OR (
                partition_schema IS NOT NULL
                AND partition_schema_oid IS NOT NULL
                AND parent_oid IS NOT NULL
                AND lower_bound IS NOT NULL
                AND upper_bound IS NOT NULL
                AND upper_bound = lower_bound + 7
                AND lower_bound = date_trunc('week', lower_bound)::date
                AND partition_table =
                    'batch_requests_archive_y' || to_char(lower_bound, 'IYYY')
                        || 'w' || to_char(lower_bound, 'IW')
            )
        )
    )
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
                || to_char(target_delete_on, 'YYYYMMDD'),
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
                    || to_char(target_delete_on, 'YYYYMMDD'),
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

-- ---------------------------------------------------------------------------
-- Generation-2 request template storage.
--
-- Weekly range-partitioned template generation, its content-free
-- routing/lifecycle metadata, and partition helpers. Nothing here scans,
-- rewrites, locks, or migrates the existing `request_templates` heap; that
-- relation becomes the frozen legacy generation once writes cut over (a
-- separately gated runtime change). Deletion is whole-partition drop, gated
-- on reference proofs over files/batches metadata.
-- ---------------------------------------------------------------------------

CREATE TABLE request_templates_g2 (
    created_on DATE NOT NULL,
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
    PRIMARY KEY (created_on, id)
) PARTITION BY RANGE (created_on);

COMMENT ON TABLE request_templates_g2 IS
    'Generation-2 request template payloads in weekly created_on partitions; scheduled deletion is whole-partition drop.';

-- Mirror the legacy read paths so per-file streaming, model grouping, and
-- custom-id lookups keep their index shapes inside each weekly child.
CREATE INDEX idx_request_templates_g2_file_line
    ON request_templates_g2 (file_id, line_number);
CREATE INDEX idx_request_templates_g2_file_model
    ON request_templates_g2 (file_id, model);
CREATE INDEX idx_request_templates_g2_model
    ON request_templates_g2 (model);
CREATE INDEX idx_request_templates_g2_custom_id
    ON request_templates_g2 (custom_id);

CREATE TRIGGER update_request_templates_g2_updated_at
    BEFORE UPDATE ON request_templates_g2
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- One lifecycle row per weekly partition. `retiring` fences reads before any
-- destructive DDL; completed rows are permanent content-free tombstones.
CREATE TABLE request_template_buckets (
    week_start DATE PRIMARY KEY
        CHECK (week_start = date_trunc('week', week_start)::date),
    partition_schema TEXT NOT NULL,
    partition_table TEXT NOT NULL,
    partition_oid OID NOT NULL,
    state TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'retiring', 'retired')),
    state_changed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (partition_schema, partition_table)
);

COMMENT ON TABLE request_template_buckets IS
    'Read fence and lifecycle registry for weekly generation-2 template partitions.';

-- Content-free location oracle: a template id resolves to exactly one
-- generation. A route row exists only for generation-2 templates; absence
-- means the legacy heap. Routes are removed in bounded chunks after their
-- bucket physically retires.
CREATE TABLE request_template_routes (
    template_id UUID PRIMARY KEY,
    week_start DATE NOT NULL REFERENCES request_template_buckets(week_start)
);
CREATE INDEX idx_request_template_routes_bucket
    ON request_template_routes (week_start, template_id);

COMMENT ON TABLE request_template_routes IS
    'Maps generation-2 template ids to their weekly partition; contains no template content.';

CREATE FUNCTION ensure_request_template_partition(
    target_week_start DATE,
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
            MESSAGE = 'relation_schema must match the template helper schema';
    END IF;

    IF target_week_start IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = 'null_value_not_allowed',
            MESSAGE = 'target_week_start must not be null';
    END IF;

    IF target_week_start <> date_trunc('week', target_week_start)::date THEN
        RAISE EXCEPTION USING
            ERRCODE = 'invalid_parameter_value',
            MESSAGE = 'target_week_start must be an ISO week Monday';
    END IF;

    partition_name := 'request_templates_g2_y'
        || to_char(target_week_start, 'IYYY')
        || 'w' || to_char(target_week_start, 'IW');
    partition_end := target_week_start + 7;
    parent_oid := to_regclass(format('%I.request_templates_g2', schema_name));
    IF parent_oid IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = 'undefined_table',
            MESSAGE = 'generation-2 template parent is missing';
    END IF;

    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            'request_templates_g2.partition:' || schema_name || ':'
                || to_char(target_week_start, 'YYYYMMDD'),
            0
        )
    );

    child_oid := to_regclass(format('%I.%I', schema_name, partition_name));
    IF child_oid IS NULL THEN
        IF EXISTS (
            SELECT 1
            FROM request_template_buckets bucket
            WHERE bucket.week_start = target_week_start
        ) THEN
            RAISE EXCEPTION USING
                ERRCODE = 'object_not_in_prerequisite_state',
                MESSAGE = 'template bucket identity exists without its partition';
        END IF;

        EXECUTE format(
            'CREATE TABLE %I.%I (LIKE %I.request_templates_g2 INCLUDING ALL)',
            schema_name,
            partition_name,
            schema_name
        );
        EXECUTE format(
            'ALTER TABLE %I.%I ADD CONSTRAINT %I CHECK '
                || '(created_on >= %L::date AND created_on < %L::date)',
            schema_name,
            partition_name,
            partition_name || '_created_on_bounds',
            target_week_start,
            partition_end
        );
        EXECUTE format(
            'ALTER TABLE %I.request_templates_g2 ATTACH PARTITION %I.%I '
                || 'FOR VALUES FROM (%L) TO (%L)',
            schema_name,
            schema_name,
            partition_name,
            target_week_start,
            partition_end
        );
        child_oid := to_regclass(format('%I.%I', schema_name, partition_name));

        INSERT INTO request_template_buckets (
            week_start,
            partition_schema,
            partition_table,
            partition_oid
        ) VALUES (
            target_week_start,
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
              target_week_start,
              partition_end
          )
    ) INTO attached_with_exact_bounds;
    IF NOT attached_with_exact_bounds THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'template partition has unexpected attachment or bounds';
    END IF;

    INSERT INTO request_template_buckets (
        week_start,
        partition_schema,
        partition_table,
        partition_oid
    ) VALUES (
        target_week_start,
        schema_name,
        partition_name,
        child_oid
    )
    ON CONFLICT (week_start) DO NOTHING;

    SELECT EXISTS (
        SELECT 1
        FROM request_template_buckets bucket
        WHERE bucket.week_start = target_week_start
          AND bucket.partition_schema = schema_name
          AND bucket.partition_table = partition_name
          AND bucket.partition_oid = child_oid
          AND bucket.state = 'active'
    ) INTO bucket_matches;
    IF NOT bucket_matches THEN
        RAISE EXCEPTION USING
            ERRCODE = 'object_not_in_prerequisite_state',
            MESSAGE = 'template bucket identity is fenced or inconsistent';
    END IF;
END;
$$;

COMMENT ON FUNCTION ensure_request_template_partition(DATE, TEXT) IS
    'Idempotently creates and records one exact weekly generation-2 template partition using a transaction advisory lock.';

CREATE FUNCTION ensure_request_template_partitions(
    first_week_start DATE,
    weeks_ahead INTEGER
)
RETURNS INTEGER
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
DECLARE
    target_week_start DATE;
    partition_name TEXT;
    offset_weeks INTEGER;
    created INTEGER := 0;
BEGIN
    IF first_week_start IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = 'null_value_not_allowed',
            MESSAGE = 'first_week_start must not be null';
    END IF;
    IF weeks_ahead < 0 THEN
        RAISE EXCEPTION USING
            ERRCODE = 'invalid_parameter_value',
            MESSAGE = 'weeks_ahead must not be negative';
    END IF;

    FOR offset_weeks IN 0..weeks_ahead LOOP
        target_week_start :=
            date_trunc('week', first_week_start)::date + offset_weeks * 7;
        partition_name := 'request_templates_g2_y'
            || to_char(target_week_start, 'IYYY')
            || 'w' || to_char(target_week_start, 'IW');
        PERFORM pg_advisory_xact_lock(
            hashtextextended(
                'request_templates_g2.partition:' || current_schema() || ':'
                    || to_char(target_week_start, 'YYYYMMDD'),
                0
            )
        );
        IF to_regclass(format('%I.%I', current_schema(), partition_name)) IS NULL THEN
            created := created + 1;
        END IF;
        PERFORM ensure_request_template_partition(target_week_start, current_schema());
    END LOOP;

    RETURN created;
END;
$$;

COMMENT ON FUNCTION ensure_request_template_partitions(DATE, INTEGER) IS
    'Ensures a continuous weekly generation-2 template partition runway and returns how many partitions were newly created.';

-- The claim path and other active-template readers already resolve through
-- this view, so widening it across both generations makes every consumer
-- generation-transparent before any write moves. The generation-2 arm is
-- empty until the write cutover is enabled, and PostgreSQL prunes fenced
-- buckets out via the state predicate.
DROP VIEW IF EXISTS active_request_templates;
CREATE VIEW active_request_templates AS
-- Legacy arm: preserves 20260507000000's semantics exactly — dedicated
-- batchless templates (file_id IS NULL) stay visible to the claim join, and
-- file-backed templates disappear with their soft-deleted file.
SELECT rt.id, rt.file_id, rt.endpoint, rt.method, rt.path, rt.body, rt.model,
       rt.api_key, rt.created_at, rt.updated_at, rt.custom_id, rt.line_number,
       rt.body_byte_size, rt.metadata
FROM request_templates rt
LEFT JOIN files f ON rt.file_id = f.id
WHERE rt.file_id IS NULL OR f.deleted_at IS NULL
UNION ALL
-- The generation-2 arm resolves through the route oracle so a lookup by
-- template id probes exactly one weekly partition, and through the bucket
-- fence so retiring content disappears before any destructive DDL.
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
FROM request_template_routes route
JOIN request_template_buckets bucket
  ON bucket.week_start = route.week_start
 AND bucket.state = 'active'
JOIN request_templates_g2 g2
  ON g2.created_on >= route.week_start
 AND g2.created_on < route.week_start + 7
 AND g2.id = route.template_id
JOIN files f ON g2.file_id = f.id
WHERE f.deleted_at IS NULL;

-- Generation-transparent raw union for internal file-keyed reads (statistics,
-- streaming, request materialization). No liveness or fence predicates: it
-- mirrors direct base-table access, and both arms serve `file_id` lookups
-- from their own indexes.
CREATE VIEW request_templates_all AS
SELECT rt.id, rt.file_id, rt.endpoint, rt.method, rt.path, rt.body, rt.model,
       rt.api_key, rt.created_at, rt.updated_at, rt.custom_id, rt.line_number,
       rt.body_byte_size, rt.metadata
FROM request_templates rt
UNION ALL
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
FROM request_templates_g2 g2;

-- ---------------------------------------------------------------------------
-- Weekly batch-archive retirement metadata.
--
-- Registers every weekly `batch_requests_archive` child in a lifecycle
-- registry so archived batch response content can be deleted by journaled
-- whole-partition drops. Batch and file metadata rows are never deleted by
-- this lifecycle; only partition content is.
-- ---------------------------------------------------------------------------

CREATE TABLE batch_archive_buckets (
    week_start DATE PRIMARY KEY
        CHECK (week_start = date_trunc('week', week_start)::date),
    partition_schema TEXT NOT NULL,
    partition_table TEXT NOT NULL,
    partition_oid OID NOT NULL,
    state TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'retiring', 'retired')),
    state_changed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (partition_schema, partition_table)
);

COMMENT ON TABLE batch_archive_buckets IS
    'Read fence and lifecycle registry for weekly batch_requests_archive partitions.';

-- Register the already-attached weekly children. Catalog-only: never touches
-- partition contents. The ISO year/week in each child name converts back to
-- its Monday lower bound.
INSERT INTO batch_archive_buckets (
    week_start, partition_schema, partition_table, partition_oid
)
SELECT
    to_date(substring(child.relname FROM 'y(\d{4})w\d{2}$') || '-'
            || substring(child.relname FROM 'w(\d{2})$') || '-1',
            'IYYY-IW-ID'),
    namespace.nspname,
    child.relname,
    child.oid
FROM pg_inherits inheritance
JOIN pg_class child ON child.oid = inheritance.inhrelid
JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
WHERE inheritance.inhparent = 'batch_requests_archive'::regclass
  AND child.relname ~ '^batch_requests_archive_y\d{4}w\d{2}$'
ON CONFLICT (week_start) DO NOTHING;

-- The runway function now also registers each child it creates, and
-- registers any pre-existing unregistered child it passes over.
CREATE OR REPLACE FUNCTION ensure_archive_partitions(weeks_ahead integer DEFAULT 4)
RETURNS integer
LANGUAGE plpgsql
AS $$
DECLARE
    this_monday date := date_trunc('week', now() AT TIME ZONE 'UTC')::date;
    target date;
    part_name text;
    part_oid oid;
    created integer := 0;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtext('ensure_archive_partitions')::bigint);
    FOR i IN 0..weeks_ahead LOOP
        target := this_monday + (i * 7);
        part_name := 'batch_requests_archive_y'
            || to_char(target, 'IYYY') || 'w' || to_char(target, 'IW');
        IF to_regclass(part_name) IS NULL THEN
            EXECUTE format(
                'CREATE TABLE %I (LIKE batch_requests_archive INCLUDING ALL)',
                part_name
            );
            EXECUTE format(
                'ALTER TABLE %I ADD CONSTRAINT %I '
                'CHECK (archive_bucket >= %L AND archive_bucket < %L)',
                part_name, part_name || '_bounds', target, target + 7
            );
            EXECUTE format(
                'ALTER TABLE batch_requests_archive ATTACH PARTITION %I '
                'FOR VALUES FROM (%L) TO (%L)',
                part_name, target, target + 7
            );
            EXECUTE format(
                'ALTER TABLE %I DROP CONSTRAINT %I',
                part_name, part_name || '_bounds'
            );
            created := created + 1;
        END IF;
        part_oid := to_regclass(part_name);
        INSERT INTO batch_archive_buckets (
            week_start, partition_schema, partition_table, partition_oid
        ) VALUES (target, current_schema(), part_name, part_oid)
        ON CONFLICT (week_start) DO NOTHING;
    END LOOP;
    RETURN created;
END;
$$;
