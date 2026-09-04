-- Recreate every index dropped by the up migration, verbatim from the
-- production definitions captured on 2026-09-03.
--
-- On a large deployment, build these CONCURRENTLY instead so the rollback does
-- not hold locks; each statement below is otherwise a plain blocking build.

CREATE INDEX IF NOT EXISTS idx_request_templates_custom_id
  ON request_templates (custom_id);

CREATE INDEX IF NOT EXISTS idx_request_templates_model
  ON request_templates (model);

CREATE INDEX IF NOT EXISTS idx_requests_created_at
  ON requests (created_at);

CREATE INDEX IF NOT EXISTS idx_requests_batch_id
  ON requests (batch_id);

CREATE INDEX IF NOT EXISTS idx_files_name_uploaded_by
  ON files (name, uploaded_by);

CREATE INDEX IF NOT EXISTS idx_files_status
  ON files (status);

CREATE INDEX IF NOT EXISTS idx_batches_active_expiration_with_id
  ON batches (expires_at)
  INCLUDE (id)
  WHERE cancelling_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_batches_output_file_id
  ON batches (output_file_id)
  WHERE output_file_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_batches_error_file_id
  ON batches (error_file_id)
  WHERE error_file_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_daemons_created_at
  ON daemons (created_at DESC);

CREATE INDEX IF NOT EXISTS idx_daemons_heartbeat
  ON daemons (last_heartbeat DESC)
  WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_daemons_status
  ON daemons (status);
