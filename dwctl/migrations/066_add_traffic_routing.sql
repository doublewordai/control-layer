-- Add per-model traffic routing rules and batch completion window controls
--
-- traffic_routing_rules: JSONB array of routing rules matching onwards RoutingRule format
--   Each rule has match_labels (key-value pairs to match against API key labels)
--   and an action (deny or redirect to another model alias).
--   Example: [{"match_labels": {"purpose": "playground"}, "action": {"type": "deny"}},
--             {"match_labels": {"purpose": "batch"}, "action": {"type": "redirect", "target": "gpt-4o-mini"}}]
--
-- allowed_batch_completion_windows: Per-model override for which batch completion
--   windows are accepted. NULL means use the global config default.
--   Example: ['24h'] to only allow 24-hour batches on an expensive model.

ALTER TABLE deployed_models
ADD COLUMN IF NOT EXISTS traffic_routing_rules JSONB,
ADD COLUMN IF NOT EXISTS allowed_batch_completion_windows TEXT[];
