-- Rollback migration for initial schema

-- Drop triggers
DROP TRIGGER IF EXISTS request_update_notify ON requests;
DROP TRIGGER IF EXISTS update_requests_updated_at ON requests;

-- Drop functions
DROP FUNCTION IF EXISTS notify_request_update();
DROP FUNCTION IF EXISTS update_updated_at_column();

-- Drop table
DROP TABLE IF EXISTS requests;

-- Drop enum type
DROP TYPE IF EXISTS request_state;
