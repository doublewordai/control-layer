-- Removing this index can make trailing demand scans expensive on populated
-- retained-response partitions. Account for that cost when rolling back.
SET LOCAL lock_timeout = '5s';

DROP INDEX IF EXISTS idx_retained_response_objects_state_terminal;
