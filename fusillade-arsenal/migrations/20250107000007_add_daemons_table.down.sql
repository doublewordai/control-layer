-- Drop daemons table and related objects
DROP TRIGGER IF EXISTS update_daemons_updated_at ON daemons;
DROP INDEX IF EXISTS idx_daemons_created_at;
DROP INDEX IF EXISTS idx_daemons_heartbeat;
DROP INDEX IF EXISTS idx_daemons_status;
DROP TABLE IF EXISTS daemons;
