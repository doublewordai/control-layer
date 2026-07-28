\set ON_ERROR_STOP on
\timing on

-- Pre-deploy preparation for Fusillade migration
-- 20260722000000_add_background_service_tier.
--
-- Keep the application on the release before background inference support,
-- then run:
--
--   psql "$DATABASE_URL" \
--     -X \
--     -v apply_background_indexes=1 \
--     -f scripts/predeploy_background_service_tier_indexes.sql
--
-- DATABASE_URL must select the intended database. The script prints the
-- target before making changes.
--
-- Monitor the active build from a second terminal:
--
--   psql "$DATABASE_URL" -X -c "
--     SELECT
--       p.phase,
--       p.blocks_done,
--       p.blocks_total,
--       round(100.0 * p.blocks_done / NULLIF(p.blocks_total, 0), 2) AS blocks_pct,
--       c.relname AS index_name
--     FROM pg_stat_progress_create_index AS p
--     LEFT JOIN pg_class AS c ON c.oid = p.index_relid;
--   "
--
-- Do not wrap this file in BEGIN/COMMIT. CREATE INDEX CONCURRENTLY must run
-- in autocommit mode. The corresponding migration uses IF NOT EXISTS, so
-- these expensive builds become no-ops during application startup.

\if :{?apply_background_indexes}
    \if :apply_background_indexes
    \else
        DO $confirmation$
        BEGIN
            RAISE EXCEPTION
                'Refusing to run: apply_background_indexes must be true';
        END
        $confirmation$;
    \endif
\else
    DO $confirmation$
    BEGIN
        RAISE EXCEPTION
            'Refusing to run without -v apply_background_indexes=1';
    END
    $confirmation$;
\endif

\echo 'Target database:'
SELECT
    current_database() AS database,
    current_user AS database_user,
    inet_server_addr() AS server_address,
    inet_server_port() AS server_port;

-- Fail before expensive work if this is not the expected Fusillade schema.
DO $preflight$
BEGIN
    IF to_regclass('fusillade.requests') IS NULL THEN
        RAISE EXCEPTION 'fusillade.requests does not exist';
    END IF;

    IF to_regclass('fusillade.batches') IS NULL THEN
        RAISE EXCEPTION 'fusillade.batches does not exist';
    END IF;

    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'fusillade'
          AND table_name = 'requests'
          AND column_name = 'service_tier'
          AND data_type = 'text'
    ) THEN
        RAISE EXCEPTION 'fusillade.requests.service_tier text column does not exist';
    END IF;
END
$preflight$;

-- This nullable column is backward-compatible with the previous application
-- release and is required by idx_batches_background_active. Adding it is a
-- metadata-only operation, but it briefly needs ACCESS EXCLUSIVE, so fail
-- quickly rather than becoming a lock barrier behind existing traffic.
SET lock_timeout = '5s';
ALTER TABLE fusillade.batches
    ADD COLUMN IF NOT EXISTS service_tier TEXT;
RESET lock_timeout;

DO $column_check$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'fusillade'
          AND table_name = 'batches'
          AND column_name = 'service_tier'
          AND data_type = 'text'
    ) THEN
        RAISE EXCEPTION 'fusillade.batches.service_tier is not a text column';
    END IF;
END
$column_check$;

-- An interrupted concurrent build can leave an unusable same-name index.
-- Remove only those invalid/not-ready remnants before retrying.
SELECT format(
    'DROP INDEX CONCURRENTLY IF EXISTS %I.%I;',
    index_namespace.nspname,
    index_class.relname
)
FROM pg_index AS index_state
JOIN pg_class AS index_class
  ON index_class.oid = index_state.indexrelid
JOIN pg_namespace AS index_namespace
  ON index_namespace.oid = index_class.relnamespace
WHERE index_namespace.nspname = 'fusillade'
  AND index_class.relname = ANY (ARRAY[
      'idx_batches_background_active',
      'idx_requests_pending_background_batchless',
      'idx_requests_pending_background_batched',
      'idx_requests_pending_batchless_sla',
      'idx_requests_active_sla_counts'
  ])
  AND (
      NOT index_state.indisready
      OR NOT index_state.indisvalid
      OR NOT index_state.indislive
  )
\gexec

SET statement_timeout = 0;

\echo '[1/5] Building idx_batches_background_active'
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_background_active
ON fusillade.batches (created_at, id)
WHERE service_tier = 'background'
  AND deleted_at IS NULL;

\echo '[2/5] Building idx_requests_pending_background_batchless'
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_pending_background_batchless
ON fusillade.requests (model, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NULL
  AND template_id IS NOT NULL
  AND service_tier = 'background';

\echo '[3/5] Building idx_requests_pending_background_batched'
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_pending_background_batched
ON fusillade.requests (model, batch_id, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NOT NULL
  AND template_id IS NOT NULL
  AND service_tier = 'background';

\echo '[4/5] Building idx_requests_pending_batchless_sla'
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_pending_batchless_sla
ON fusillade.requests (model, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NULL
  AND template_id IS NOT NULL
  AND service_tier IS DISTINCT FROM 'background';

\echo '[5/5] Building idx_requests_active_sla_counts'
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_sla_counts
ON fusillade.requests (batch_id, model)
WHERE state IN ('pending', 'claimed', 'processing')
  AND template_id IS NOT NULL
  AND (
      service_tier IS NULL
      OR service_tier NOT IN ('priority', 'background')
  );

DO $validation$
DECLARE
    valid_index_count INTEGER;
BEGIN
    SELECT count(*)
    INTO valid_index_count
    FROM pg_index AS index_state
    JOIN pg_class AS index_class
      ON index_class.oid = index_state.indexrelid
    JOIN pg_namespace AS index_namespace
      ON index_namespace.oid = index_class.relnamespace
    WHERE index_namespace.nspname = 'fusillade'
      AND index_class.relname = ANY (ARRAY[
          'idx_batches_background_active',
          'idx_requests_pending_background_batchless',
          'idx_requests_pending_background_batched',
          'idx_requests_pending_batchless_sla',
          'idx_requests_active_sla_counts'
      ])
      AND index_state.indisready
      AND index_state.indisvalid
      AND index_state.indislive;

    IF valid_index_count <> 5 THEN
        RAISE EXCEPTION
            'Expected 5 ready, valid, live background-tier indexes; found %',
            valid_index_count;
    END IF;
END
$validation$;

\echo 'Background-tier index preparation complete:'
SELECT
    index_class.relname AS index_name,
    pg_size_pretty(pg_relation_size(index_class.oid)) AS index_size,
    index_state.indisready AS ready,
    index_state.indisvalid AS valid,
    index_state.indislive AS live
FROM pg_index AS index_state
JOIN pg_class AS index_class
  ON index_class.oid = index_state.indexrelid
JOIN pg_namespace AS index_namespace
  ON index_namespace.oid = index_class.relnamespace
WHERE index_namespace.nspname = 'fusillade'
  AND index_class.relname = ANY (ARRAY[
      'idx_batches_background_active',
      'idx_requests_pending_background_batchless',
      'idx_requests_pending_background_batched',
      'idx_requests_pending_batchless_sla',
      'idx_requests_active_sla_counts'
  ])
ORDER BY index_class.relname;
