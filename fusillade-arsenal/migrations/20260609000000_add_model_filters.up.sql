-- Async model-filters claim gate (Solution 5, COR-432).
--
-- `model_filters` is an APPEND-ONLY EVENT LOG of per-model internal liveness
-- transitions. Each row records that, at `created_at`, a model became `live`
-- (serving now), `coming` (will serve, with an ETA in `expected_ready_at`), or
-- `absent` (an explicit tombstone: the controller is no longer deploying the model).
--
-- The CURRENT state of a model is the LATEST event (max `created_at`) for that
-- model. A model is treated as absent if its latest event has `state='absent'`
-- OR it has no events at all. The daemon's claim cycle consults the latest
-- event per model to decide, per request against an effective deadline,
-- whether to claim now (dispatch -> onwards routes internal/OpenRouter) or hold
-- the request in the queue waiting for imminent internal capacity.
--
-- Append-only (rather than current-state upsert) lets us learn model load
-- durations from the log: pairing each `coming` event with the next `live`
-- event for that model yields observed load times (see `model_load_estimate`),
-- which the controller uses to set future ETAs. Retraction is appending an `absent`
-- event, never a DELETE.
--
-- The controller is responsible for appending only on a state CHANGE (so the log
-- stays a transition log, not a poll log). Old events are purged by the daemon
-- while always retaining the latest event per model (see the purge task).
--
-- The controller owns the `state`/`expected_ready_at` invariant
-- (`expected_ready_at` set iff `state='coming'`). We deliberately do NOT add a
-- hard CHECK for it: this is an append-only event log of historical
-- transitions, and the claim gate only reads `expected_ready_at` when
-- `state='coming'`, so a stray value on a non-`coming` event is inert.
CREATE TABLE model_filters (
    id                BIGSERIAL PRIMARY KEY,
    model             TEXT NOT NULL,
    state             TEXT NOT NULL CHECK (state IN ('live', 'coming', 'absent')),
    expected_ready_at TIMESTAMPTZ, -- set when state = 'coming' (producer-owned invariant)
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Latest-event lookup per model (the claim query's LATERAL ORDER BY
-- created_at DESC, id DESC LIMIT 1) and the retention purge both walk events
-- for a single model newest-first. `id DESC` is in the index so the latest
-- event is deterministic (and the ORDER BY is index-satisfiable) when a batch
-- append shares one `created_at` across rows.
CREATE INDEX idx_model_filters_model_created_at
    ON model_filters (model, created_at DESC, id DESC);

-- No separate liveness/heartbeat table: the claim gate trusts the latest
-- `model_filters` event per model (live / no-events => claim at full capacity;
-- coming / absent => leaky-bucket trickle + per-request deadline ramp) and
-- degrades per-request. A not-live model frozen by a dead/quiet controller
-- still drains: each held request claims at full capacity once within its
-- deadline ramp, so work is always eventually released without a heartbeat.
