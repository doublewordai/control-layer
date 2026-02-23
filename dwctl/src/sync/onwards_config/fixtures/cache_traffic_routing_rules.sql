-- Fixture: add per-model traffic routing rules for cache-shape assertions.

-- regular-private: batch -> deny, realtime -> redirect to regular-public
INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action, redirect_target_id)
VALUES
    ('40000000-0000-0000-0000-000000000002', 'batch', 'deny', NULL),
    ('40000000-0000-0000-0000-000000000002', 'realtime', 'redirect', '40000000-0000-0000-0000-000000000001');

-- composite-priority: batch -> redirect to escalation-private, realtime -> deny
INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action, redirect_target_id)
VALUES
    ('50000000-0000-0000-0000-000000000001', 'batch', 'redirect', '40000000-0000-0000-0000-000000000004'),
    ('50000000-0000-0000-0000-000000000001', 'realtime', 'deny', NULL);
