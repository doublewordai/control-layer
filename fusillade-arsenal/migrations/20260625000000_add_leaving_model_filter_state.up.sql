-- Add the `leaving` state to model_filters (graceful scale-down / drain).
--
-- `leaving` marks a model the controller has decided to scale to zero but whose
-- workers are still up finishing in-flight requests (so it is still listed in the
-- gateway `/v1/models`). It sits between `live` and `absent` in the teardown
-- lifecycle:
--     live -> leaving (draining, still serving) -> absent (workers gone)
--
-- The claim gate treats `leaving` as NOT-LIVE, identical to `coming`/`absent`:
-- the gate only claims at full capacity when `state = 'live'` (or a model has no
-- events at all), so any non-`live` value already falls onto the leaky-bucket +
-- deadline-ramp path with NO query change. This migration only widens the CHECK
-- constraint to admit the new value.
ALTER TABLE model_filters DROP CONSTRAINT model_filters_state_check;
ALTER TABLE model_filters
    ADD CONSTRAINT model_filters_state_check
    CHECK (state IN ('live', 'coming', 'leaving', 'absent'));
