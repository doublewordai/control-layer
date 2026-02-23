-- Positive balance for user B only.
INSERT INTO credits_transactions (
    id, user_id, transaction_type, amount, source_id, balance_after, description
)
VALUES
    (
        '90000000-0000-0000-0000-000000000002',
        '00000000-0000-0000-0000-0000000000b1',
        'admin_grant',
        100.0,
        'cache-balance-seed-b',
        100.0,
        'Fixture credits for user B'
    );
