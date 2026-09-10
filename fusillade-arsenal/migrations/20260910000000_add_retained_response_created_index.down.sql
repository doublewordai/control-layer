-- Removing this index returns the unscoped Responses page to sorting the whole
-- retained history: measured at 124.6s on a copy of production, against 2.2ms
-- with the index. Account for that before rolling back.
SET LOCAL lock_timeout = '5s';

DROP INDEX IF EXISTS idx_retained_response_objects_created;
