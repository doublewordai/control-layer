-- Base fixture for onwards cache-shape tests.
-- Defines users, groups, API keys, endpoints, deployments, and composite components.

-- Users
INSERT INTO users (id, username, email, display_name, auth_source, is_admin, is_deleted, is_internal)
VALUES
    ('00000000-0000-0000-0000-0000000000a1', 'cache_user_a', 'cache_user_a@example.com', 'Cache User A', 'test', false, false, false),
    ('00000000-0000-0000-0000-0000000000b1', 'cache_user_b', 'cache_user_b@example.com', 'Cache User B', 'test', false, false, false),
    ('00000000-0000-0000-0000-0000000000c1', 'cache_batch_owner', 'cache_batch_owner@example.com', 'Cache Batch Owner', 'test', false, false, false);

-- Groups
INSERT INTO groups (id, name, description, created_by, source)
VALUES
    ('00000000-0000-0000-0000-000000000aa1', 'cache-private-a', 'Private cache group A', '00000000-0000-0000-0000-000000000000', 'native');

-- User group membership
INSERT INTO user_groups (id, user_id, group_id)
VALUES
    ('10000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-000000000aa1');

-- API keys
INSERT INTO api_keys (id, name, description, secret, user_id, hidden, purpose, requests_per_second, burst_size)
VALUES
    ('20000000-0000-0000-0000-0000000000a1', 'cache-key-a', 'Realtime key for user A', 'sk-cache-a', '00000000-0000-0000-0000-0000000000a1', false, 'realtime', NULL, NULL),
    ('20000000-0000-0000-0000-0000000000b1', 'cache-key-b', 'Realtime key for user B', 'sk-cache-b', '00000000-0000-0000-0000-0000000000b1', false, 'realtime', NULL, NULL),
    ('20000000-0000-0000-0000-0000000000c1', 'cache-batch-key', 'Batch key for escalation tests', 'sk-cache-batch', '00000000-0000-0000-0000-0000000000c1', false, 'batch', NULL, NULL);

-- Endpoints
INSERT INTO inference_endpoints (
    id, name, description, url, created_by, model_filter, api_key, auth_header_name, auth_header_prefix
)
VALUES
    (
        '30000000-0000-0000-0000-000000000001',
        'cache-endpoint-default',
        'Default auth endpoint',
        'https://api.default.example.com/v1',
        '00000000-0000-0000-0000-000000000000',
        NULL,
        NULL,
        'Authorization',
        'Bearer '
    ),
    (
        '30000000-0000-0000-0000-000000000002',
        'cache-endpoint-custom',
        'Custom auth endpoint',
        'https://api.custom.example.com/router',
        '00000000-0000-0000-0000-000000000000',
        NULL,
        NULL,
        'X-API-Key',
        'Token '
    );

-- Regular deployments
INSERT INTO deployed_models (
    id, model_name, alias, description, type, capabilities, created_by, hosted_on,
    requests_per_second, burst_size, status, last_sync, deleted, capacity, batch_capacity,
    is_composite, lb_strategy, fallback_enabled, fallback_on_rate_limit, fallback_on_status,
    sanitize_responses, throughput, fallback_with_replacement, fallback_max_attempts
)
VALUES
    (
        '40000000-0000-0000-0000-000000000001',
        'regular-public-model',
        'regular-public',
        'Public regular model',
        NULL,
        NULL,
        '00000000-0000-0000-0000-000000000000',
        '30000000-0000-0000-0000-000000000001',
        NULL,
        NULL,
        'active',
        NOW(),
        false,
        NULL,
        NULL,
        false,
        NULL,
        NULL,
        NULL,
        NULL,
        false,
        NULL,
        NULL,
        NULL
    ),
    (
        '40000000-0000-0000-0000-000000000002',
        'regular-private-model',
        'regular-private',
        'Private regular model',
        NULL,
        NULL,
        '00000000-0000-0000-0000-000000000000',
        '30000000-0000-0000-0000-000000000002',
        12.0,
        20,
        'active',
        NOW(),
        false,
        6,
        NULL,
        false,
        NULL,
        NULL,
        NULL,
        NULL,
        true,
        NULL,
        NULL,
        NULL
    ),
    (
        '40000000-0000-0000-0000-000000000003',
        'metered-public-model',
        'metered-public',
        'Public model with tariff',
        NULL,
        NULL,
        '00000000-0000-0000-0000-000000000000',
        '30000000-0000-0000-0000-000000000001',
        NULL,
        NULL,
        'active',
        NOW(),
        false,
        NULL,
        NULL,
        false,
        NULL,
        NULL,
        NULL,
        NULL,
        false,
        NULL,
        NULL,
        NULL
    ),
    (
        '40000000-0000-0000-0000-000000000004',
        'escalation-private-model',
        'escalation-private',
        'No group access; batch escalation only',
        NULL,
        NULL,
        '00000000-0000-0000-0000-000000000000',
        '30000000-0000-0000-0000-000000000001',
        NULL,
        NULL,
        'active',
        NOW(),
        false,
        NULL,
        NULL,
        false,
        NULL,
        NULL,
        NULL,
        NULL,
        false,
        NULL,
        NULL,
        NULL
    ),
    (
        '40000000-0000-0000-0000-000000000005',
        'component-a-model',
        'component-a',
        'Composite provider A',
        NULL,
        NULL,
        '00000000-0000-0000-0000-000000000000',
        '30000000-0000-0000-0000-000000000001',
        30.0,
        60,
        'active',
        NOW(),
        false,
        4,
        NULL,
        false,
        NULL,
        NULL,
        NULL,
        NULL,
        false,
        NULL,
        NULL,
        NULL
    ),
    (
        '40000000-0000-0000-0000-000000000006',
        'component-b-model',
        'component-b',
        'Composite provider B',
        NULL,
        NULL,
        '00000000-0000-0000-0000-000000000000',
        '30000000-0000-0000-0000-000000000002',
        40.0,
        80,
        'active',
        NOW(),
        false,
        5,
        NULL,
        false,
        NULL,
        NULL,
        NULL,
        NULL,
        false,
        NULL,
        NULL,
        NULL
    );

-- Composite deployment
INSERT INTO deployed_models (
    id, model_name, alias, description, type, capabilities, created_by, hosted_on,
    requests_per_second, burst_size, status, last_sync, deleted, capacity, batch_capacity,
    is_composite, lb_strategy, fallback_enabled, fallback_on_rate_limit, fallback_on_status,
    sanitize_responses, throughput, fallback_with_replacement, fallback_max_attempts
)
VALUES
    (
        '50000000-0000-0000-0000-000000000001',
        'composite-priority-model',
        'composite-priority',
        'Composite model with explicit fallback settings',
        NULL,
        NULL,
        '00000000-0000-0000-0000-000000000000',
        NULL,
        21.0,
        42,
        'active',
        NOW(),
        false,
        9,
        NULL,
        true,
        'priority',
        true,
        false,
        '{429,503}',
        true,
        NULL,
        true,
        2
    );

-- Deployment group access
INSERT INTO deployment_groups (id, deployment_id, group_id, granted_by)
VALUES
    (
        '60000000-0000-0000-0000-000000000001',
        '40000000-0000-0000-0000-000000000001',
        '00000000-0000-0000-0000-000000000000',
        '00000000-0000-0000-0000-000000000000'
    ),
    (
        '60000000-0000-0000-0000-000000000002',
        '40000000-0000-0000-0000-000000000002',
        '00000000-0000-0000-0000-000000000aa1',
        '00000000-0000-0000-0000-000000000000'
    ),
    (
        '60000000-0000-0000-0000-000000000003',
        '40000000-0000-0000-0000-000000000003',
        '00000000-0000-0000-0000-000000000000',
        '00000000-0000-0000-0000-000000000000'
    ),
    (
        '60000000-0000-0000-0000-000000000004',
        '50000000-0000-0000-0000-000000000001',
        '00000000-0000-0000-0000-000000000aa1',
        '00000000-0000-0000-0000-000000000000'
    );

-- Composite components (priority order via sort_order)
INSERT INTO deployed_model_components (
    id, composite_model_id, deployed_model_id, weight, enabled, sort_order
)
VALUES
    (
        '70000000-0000-0000-0000-000000000001',
        '50000000-0000-0000-0000-000000000001',
        '40000000-0000-0000-0000-000000000006',
        30,
        true,
        0
    ),
    (
        '70000000-0000-0000-0000-000000000002',
        '50000000-0000-0000-0000-000000000001',
        '40000000-0000-0000-0000-000000000005',
        70,
        true,
        1
    );
