-- Reverse the `leaving` state. `leaving` is a transient draining state on the way
-- to `absent`; collapse any such events to `absent` (which preserves the not-live
-- semantics of the latest event per model) before narrowing the CHECK constraint
-- back to the original three states. Without this, narrowing would fail if any
-- `leaving` rows exist.
UPDATE model_filters SET state = 'absent' WHERE state = 'leaving';
ALTER TABLE model_filters DROP CONSTRAINT model_filters_state_check;
ALTER TABLE model_filters
    ADD CONSTRAINT model_filters_state_check
    CHECK (state IN ('live', 'coming', 'absent'));
