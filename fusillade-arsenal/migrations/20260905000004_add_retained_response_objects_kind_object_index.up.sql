-- The archive move's cross-day duplicate check asks whether a group or
-- request id already exists under any other day. Every other index on the
-- retained store leads with delete_on, so that probe had to scan each day's
-- primary key; this index answers it by id directly.
--
-- Plain CREATE INDEX (transactional): a partitioned parent cannot be built
-- CONCURRENTLY, and daily partitions are created from the parent with
-- LIKE ... INCLUDING ALL, so new days inherit it. Where an operator has
-- already built the identical parent-and-children index by hand (parent ON
-- ONLY, children concurrently, then attached), IF NOT EXISTS makes this a
-- no-op; on a fresh database the partitions are empty and the build is
-- immediate.
CREATE INDEX IF NOT EXISTS idx_retained_response_objects_kind_object
    ON retained_response_objects (object_kind, object_id);
