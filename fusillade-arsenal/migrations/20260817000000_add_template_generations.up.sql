-- Generation-2 request template storage.
--
-- Expand-only: adds a weekly range-partitioned template generation, its
-- content-free routing/lifecycle metadata, and partition helpers. Nothing in
-- this migration scans, rewrites, locks, or migrates the existing
-- `request_templates` heap; that relation becomes the frozen legacy
-- generation once writes cut over (a later, separately gated change).
--
-- The deletion model is partition drop only: a weekly bucket becomes
-- droppable when reference gates over files/batches metadata prove no reader
-- can need it, and the legacy heap is eventually dropped as one relation.
-- No scheduled row-by-row template deletion exists in any generation.

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
SELECT rt.id, rt.file_id, rt.endpoint, rt.method, rt.path, rt.body, rt.model,
       rt.api_key, rt.created_at, rt.updated_at, rt.custom_id, rt.line_number,
       rt.body_byte_size, rt.metadata
FROM request_templates rt
JOIN files f ON rt.file_id = f.id
WHERE f.deleted_at IS NULL
UNION ALL
SELECT g2.id, g2.file_id, g2.endpoint, g2.method, g2.path, g2.body, g2.model,
       g2.api_key, g2.created_at, g2.updated_at, g2.custom_id, g2.line_number,
       g2.body_byte_size, g2.metadata
FROM request_templates_g2 g2
JOIN files f ON g2.file_id = f.id
JOIN request_template_buckets bucket
  ON g2.created_on >= bucket.week_start
 AND g2.created_on < bucket.week_start + 7
WHERE f.deleted_at IS NULL
  AND bucket.state = 'active';
