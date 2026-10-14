-- Per-organisation, per-model realtime priority bias.
--
-- With deadline-ordered realtime priority enabled
-- (`scheduling.inject_realtime_priority`), every realtime request sent to a
-- dynamo frontend carries `nvext.agent_hints.priority = -T`, where T is the
-- request's effective time in Unix seconds and the earliest T is served
-- first. Standard realtime uses T = arrival time. An organisation's overlay on
-- a model may move its realtime requests on that model earlier by this many
-- seconds: T = arrival - priority_bias_seconds. Batch work keeps its own
-- T = batch deadline.
--
-- Declared in the per-organisation catalog files (`priority_bias_seconds` on a
-- model entry) and materialised here like the overlay's other fields. NULL =
-- no bias. Bounded to one day so a typo cannot push an organisation's
-- realtime work ahead of every batch deadline in the queue.
--
-- Additive: a nullable column with no default is a metadata-only change, the
-- previous release neither reads nor writes it (its overlay queries name
-- their columns), and the CHECK only has NULLs to validate on a small table.
-- The existing model_overlays_notify trigger already fires on every row
-- change, so a changed bias reaches onwards through the usual config sync.
SET LOCAL lock_timeout = '5s';

ALTER TABLE model_overlays
    ADD COLUMN IF NOT EXISTS priority_bias_seconds INTEGER;

ALTER TABLE model_overlays
    ADD CONSTRAINT chk_model_overlays_priority_bias_seconds
    CHECK (priority_bias_seconds IS NULL OR priority_bias_seconds BETWEEN 0 AND 86400);

COMMENT ON COLUMN model_overlays.priority_bias_seconds IS
  'Seconds this organisation''s realtime requests on this model are moved earlier in the deadline-ordered dynamo priority (T = arrival - bias). NULL = no bias. Applied only when scheduling.inject_realtime_priority is on. 0..86400.';
