-- Positive balance for user A only.
--
-- Eligibility reads the user_balance_checkpoints read model, so the fixture
-- seeds both the ledger row and the corresponding checkpoint balance (as the
-- inline credit path would).
INSERT INTO credits_transactions (
    id, user_id, transaction_type, amount, source_id, balance_after, description
)
VALUES
    (
        '90000000-0000-0000-0000-000000000001',
        '00000000-0000-0000-0000-0000000000a1',
        'admin_grant',
        100.0,
        'cache-balance-seed-a',
        100.0,
        'Fixture credits for user A'
    );

INSERT INTO user_balance_checkpoints (user_id, checkpoint_seq, balance)
SELECT '00000000-0000-0000-0000-0000000000a1', MAX(seq), 100.0
FROM credits_transactions
WHERE user_id = '00000000-0000-0000-0000-0000000000a1'
ON CONFLICT (user_id) DO UPDATE SET
    checkpoint_seq = EXCLUDED.checkpoint_seq,
    balance = EXCLUDED.balance,
    updated_at = NOW();
