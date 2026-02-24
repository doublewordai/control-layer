-- Tariff fixture for metered model access tests.
INSERT INTO model_tariffs (
    id, deployed_model_id, name, input_price_per_token, output_price_per_token, api_key_purpose, valid_from, valid_until
)
VALUES
    (
        '80000000-0000-0000-0000-000000000001',
        '40000000-0000-0000-0000-000000000003',
        'metered-default',
        0.000001,
        0.000002,
        NULL,
        NOW(),
        NULL
    );
