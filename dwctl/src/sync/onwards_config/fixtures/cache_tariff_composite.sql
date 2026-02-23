-- Active non-zero tariff for the composite model.
INSERT INTO model_tariffs (
    id, deployed_model_id, name, input_price_per_token, output_price_per_token, api_key_purpose, valid_from, valid_until
)
VALUES
    (
        '81000000-0000-0000-0000-000000000001',
        '50000000-0000-0000-0000-000000000001',
        'composite-metered-default',
        0.000001,
        0.000002,
        NULL,
        NOW(),
        NULL
    );
