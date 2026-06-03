-- Materialized per-user credit balance so the onwards cache sync no longer
-- re-sums the credits_transactions ledger for every user on every reload. That
-- ledger sum dominated load_targets_from_db; reading a maintained balance makes
-- the query cheap and removes the per-reload load on the credits_transactions
-- index.
--
-- Equivalence to the previous `user_balances` CTE:
--   current_balance = checkpoint.balance + SUM(tx after checkpoint_seq)
-- and user_balance_checkpoints.balance is itself "aggregated balance of all
-- transactions up to checkpoint_seq" (see migration 047), so the maintained
-- value here == the signed SUM over the whole ledger == what the CTE computed.
--
-- credits_transactions is append-only for balance purposes, and this is enforced
-- at the database level: migration 050's prevent_credit_transaction_modification
-- trigger RAISEs on any DELETE and on any UPDATE that changes amount or
-- transaction_type (only is_aggregated / fusillade_batch_id may change). So
-- maintaining the balance on INSERT alone is provably sufficient -- a balance-
-- affecting UPDATE or a DELETE simply cannot occur.
--
-- INVARIANT: if that guard is ever relaxed to allow amount/transaction_type to
-- change or rows to be deleted, this trigger MUST be extended to handle the
-- corresponding UPDATE/DELETE, or user_balances will silently drift.

CREATE TABLE user_balances (
    user_id    UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    balance    DECIMAL(20, 9) NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

COMMENT ON TABLE user_balances IS
    'Materialized current credit balance per user (signed SUM of credits_transactions). Maintained incrementally by the credits_transactions_apply_balance trigger; read by the onwards cache sync to gate API-key access on balance > 0.';

-- Incremental maintenance. Statement-level with a transition table so the
-- batcher's bulk usage inserts cost one grouped upsert per batch, not one per row.
CREATE OR REPLACE FUNCTION apply_credits_to_user_balance() RETURNS trigger AS $$
BEGIN
    INSERT INTO user_balances (user_id, balance, updated_at)
    SELECT nt.user_id,
           SUM(CASE WHEN nt.transaction_type IN ('purchase', 'admin_grant')
                    THEN nt.amount ELSE -nt.amount END),
           NOW()
    FROM new_rows nt
    GROUP BY nt.user_id
    ON CONFLICT (user_id) DO UPDATE
        SET balance    = user_balances.balance + EXCLUDED.balance,
            updated_at = NOW();
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- Create the trigger BEFORE the backfill: CREATE TRIGGER takes a lock on
-- credits_transactions that blocks concurrent INSERTs until this migration
-- commits. Combined with the migration's transaction snapshot, that gives a
-- clean watermark -- every transaction is counted exactly once (either in the
-- backfill or by the trigger, never both, never neither).
--
-- Cost of that insert-blocking lock == the backfill duration below. The backfill
-- leverages the existing checkpoints (it sums checkpoint.balance + only the
-- post-checkpoint delta per user, not the raw ledger), so it is light and the
-- lock is brief. If the user count or per-user post-checkpoint backlog ever grows
-- enough to make that lock material, move the backfill out of it: capture
-- max(seq) as a watermark here, then backfill `seq <= watermark` additively
-- (ON CONFLICT DO UPDATE balance += ...) in a separate online pass while the
-- trigger handles `seq > watermark`.
CREATE TRIGGER credits_transactions_apply_balance
    AFTER INSERT ON credits_transactions
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION apply_credits_to_user_balance();

-- Backfill from the existing checkpoint + post-checkpoint delta (== full ledger sum).
INSERT INTO user_balances (user_id, balance)
SELECT u.id,
       COALESCE(c.balance, 0) + COALESCE(
           (SELECT SUM(CASE WHEN ct.transaction_type IN ('purchase', 'admin_grant')
                            THEN ct.amount ELSE -ct.amount END)
            FROM credits_transactions ct
            WHERE ct.user_id = u.id
              AND ct.seq > COALESCE(c.checkpoint_seq, 0)), 0)
FROM users u
LEFT JOIN user_balance_checkpoints c ON c.user_id = u.id
ON CONFLICT (user_id) DO UPDATE SET balance = EXCLUDED.balance;
