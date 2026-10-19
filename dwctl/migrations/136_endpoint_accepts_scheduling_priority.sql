-- Per-endpoint capability: does this serving stack understand the scheduling
-- `priority` request field? The dynamo frontend does (its scheduler orders by
-- it); third-party OpenAI-compatible APIs reject unknown fields (Fireworks:
-- "Extra inputs are not permitted"). Onwards strips `priority` from every
-- named-pool (e.g. `completions`) attempt whose member endpoint lacks this flag,
-- regardless of the member's position — replacing the "index 0 is dynamo"
-- assumption that broke as soon as the dynamo member was disabled and a third
-- party became the pool's first member.
--
-- Default false is the safe direction: an unflagged dynamo endpoint means resume
-- legs queue at priority 0 (degraded), never a leg rejected outright. Default-pool
-- traffic is not affected by this flag (batch/flex deadline priorities keep
-- reaching dynamo exactly as before). Set true on the dynamo frontend endpoint(s).
ALTER TABLE inference_endpoints
    ADD COLUMN accepts_scheduling_priority BOOLEAN NOT NULL DEFAULT FALSE;
