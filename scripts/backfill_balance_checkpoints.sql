-- One-time backfill for the balance read model (run once, after the deploy
-- that introduces write-time balance folding has fully rolled out).
--
-- Migration 110 makes user_balance_checkpoints total (a row per user) but
-- deliberately leaves pre-existing checkpoint values as the old opportunistic
-- cache left them, so the migration stays instant at pod startup. From this
-- release on, every writer folds its rows into the checkpoint synchronously,
-- so values only ever drift if something bypasses the writers. This script
-- does the one-time conversion:
--
--   1. aggregates batched ledger rows the retired lazy read path never
--      aggregated into batch_aggregates (users who never opened their
--      transactions page have a backlog of these),
--   2. heals every checkpoint whose balance disagrees with the sum of that
--      user's ledger rows.
--
-- Run it AFTER all pods are on the new release: that way it also mops up any
-- rows written by old-binary pods during the rolling deploy (which did not
-- fold), with no residual window.
--
-- Both statements are idempotent and safe against live traffic: heals are
-- guarded on the observed stale value, so a checkpoint a writer folds
-- concurrently is skipped rather than clobbered. If the run aborts on a
-- deadlock with a concurrent flush, or you are in any doubt, just run it
-- again - a clean ledger makes both statements no-ops.
--
-- Keep this script around: re-running it is also the way to reconcile
-- balances after any manual surgery on credits_transactions.

-- 1. Aggregate never-aggregated batched rows into batch_aggregates.
WITH pending AS (
    SELECT id
    FROM credits_transactions
    WHERE fusillade_batch_id IS NOT NULL
      AND is_aggregated = false
    FOR UPDATE SKIP LOCKED
),
marked AS (
    UPDATE credits_transactions t
    SET is_aggregated = true
    FROM pending
    WHERE t.id = pending.id
    RETURNING t.fusillade_batch_id, t.user_id, t.amount, t.seq, t.created_at
),
ins AS (
    INSERT INTO batch_aggregates (fusillade_batch_id, user_id, total_amount, transaction_count, max_seq, created_at, updated_at)
    SELECT fusillade_batch_id, user_id, SUM(amount), COUNT(*)::int, MAX(seq), MIN(created_at), NOW()
    FROM marked
    GROUP BY fusillade_batch_id, user_id
    ON CONFLICT (fusillade_batch_id) DO UPDATE SET
        total_amount = batch_aggregates.total_amount + EXCLUDED.total_amount,
        transaction_count = batch_aggregates.transaction_count + EXCLUDED.transaction_count,
        max_seq = GREATEST(batch_aggregates.max_seq, EXCLUDED.max_seq),
        updated_at = NOW()
    RETURNING 1
)
SELECT COUNT(*) AS batched_rows_aggregated FROM marked;

-- 2. Heal every checkpoint whose balance disagrees with the ledger.
WITH ledger AS (
    SELECT
        user_id,
        SUM(CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END) AS total,
        MAX(seq) AS max_seq
    FROM credits_transactions
    GROUP BY user_id
),
drift AS (
    SELECT c.user_id, c.balance AS stale_balance, COALESCE(l.total, 0) AS ledger_balance, l.max_seq
    FROM user_balance_checkpoints c
    LEFT JOIN ledger l ON l.user_id = c.user_id
    WHERE c.balance IS DISTINCT FROM COALESCE(l.total, 0)
),
healed AS (
    UPDATE user_balance_checkpoints c
    SET balance = d.ledger_balance,
        checkpoint_seq = GREATEST(c.checkpoint_seq, COALESCE(d.max_seq, 0)),
        updated_at = NOW()
    FROM drift d
    WHERE c.user_id = d.user_id
      AND c.balance = d.stale_balance
    RETURNING 1
)
SELECT COUNT(*) AS checkpoints_healed FROM healed;
