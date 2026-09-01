-- Remove request_update_notify trigger
-- This trigger fires on every INSERT/UPDATE to requests, causing significant
-- performance issues during bulk inserts (e.g., creating batches with thousands
-- of requests). The pg_notify overhead scales poorly with large batch sizes.

-- Drop the trigger
DROP TRIGGER IF EXISTS request_update_notify ON requests;

-- Drop the trigger function
DROP FUNCTION IF EXISTS notify_request_update();
