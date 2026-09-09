-- NOTE: in production this index carries the scouter demand query; dropping it
-- returns /monitoring/demand to its 60 s statement timeout on every poll. The
-- down migration exists to keep the pair reversible on development and test
-- databases.
SET LOCAL lock_timeout = '5s';

DROP INDEX IF EXISTS idx_retained_response_objects_state_terminal;
