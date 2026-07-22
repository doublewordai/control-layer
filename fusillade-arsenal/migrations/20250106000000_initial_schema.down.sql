-- Drop all objects in reverse order

DROP TRIGGER IF EXISTS request_update_notify ON requests;
DROP FUNCTION IF EXISTS notify_request_update();

DROP TRIGGER IF EXISTS update_requests_updated_at ON requests;
DROP TRIGGER IF EXISTS update_request_templates_updated_at ON request_templates;
DROP TRIGGER IF EXISTS update_files_updated_at ON files;
DROP FUNCTION IF EXISTS update_updated_at_column();

DROP VIEW IF EXISTS batch_status;

DROP INDEX IF EXISTS idx_requests_pending_claim;
DROP INDEX IF EXISTS idx_requests_created_at;
DROP INDEX IF EXISTS idx_requests_daemon;
DROP INDEX IF EXISTS idx_requests_model;
DROP INDEX IF EXISTS idx_requests_state;
DROP INDEX IF EXISTS idx_requests_template_id;
DROP INDEX IF EXISTS idx_requests_batch_id;
DROP INDEX IF EXISTS idx_batches_created_at;
DROP INDEX IF EXISTS idx_batches_file_id;
DROP INDEX IF EXISTS idx_request_templates_file_id;

DROP TABLE IF EXISTS requests;
DROP TABLE IF EXISTS batches;
DROP TABLE IF EXISTS request_templates;
DROP TABLE IF EXISTS files;
