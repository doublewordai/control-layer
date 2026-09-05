-- The permanent indexes behind scheduled content retention are built by the
-- migrations that follow this one, each with CREATE INDEX CONCURRENTLY
-- outside the migrator's transaction (one statement per file: Postgres runs
-- a multi-statement string as one implicit transaction, which CONCURRENTLY
-- refuses). They were previously documented as operator-built statements.
--
-- An interrupted concurrent build leaves an INVALID index of the same name
-- behind, which IF NOT EXISTS would then silently keep; drop such leftovers
-- here first so a retried start always ends with a valid index. Pre-building
-- the identical statements by hand is allowed and makes the builds no-ops.
DO $$
DECLARE
    stale RECORD;
BEGIN
    FOR stale IN
        SELECT c.relname
        FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = current_schema()
          AND NOT i.indisvalid
          AND c.relname IN (
              'idx_requests_batchless_retention_due',
              'idx_files_content_expiry_due',
              'idx_batches_archive_bucket_retirement'
          )
    LOOP
        EXECUTE format('DROP INDEX %I.%I', current_schema(), stale.relname);
    END LOOP;
END $$;
