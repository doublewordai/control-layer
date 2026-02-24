-- Positive balance for batch key owner to satisfy composite balance gate.
INSERT INTO credits_transactions (
    id, user_id, transaction_type, amount, source_id, balance_after, description
)
VALUES
    (
        '90000000-0000-0000-0000-000000000003',
        '00000000-0000-0000-0000-0000000000c1',
        'admin_grant',
        100.0,
        'cache-balance-seed-batch',
        100.0,
        'Fixture credits for batch key owner'
    );
