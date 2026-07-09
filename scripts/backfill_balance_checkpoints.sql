-- One-time conversion for the balance read model (run after the deploy that
-- introduces write-time balance folding has fully rolled out).
--
-- Migration 111 makes user_balance_checkpoints total (a row per user) but
-- deliberately leaves pre-existing checkpoint values as the old opportunistic
-- cache left them, so the migration stays instant at pod startup. From this
-- release on, every writer folds its rows into the checkpoint synchronously,
-- so values only ever drift if something bypasses the writers.
--
-- Step 1 (REQUIRED, minutes): heal every checkpoint whose balance disagrees
--   with the sum of that user's ledger rows. Until this runs, balances for
--   pre-existing activity are whatever the old cache left behind.
--
-- Step 2 (OPTIONAL, slow): aggregate the historical backlog of batched
--   ledger rows into batch_aggregates. This only affects the GROUPED batch
--   view of the transactions page: undrained historical batches are simply
--   absent from that view (they remain in the ledger, the flat listing, and
--   all balances). Run it whenever, at leisure, or not at all.
--
-- Run step 1 AFTER all pods are on the new release: that way it also mops up
-- any rows written by old-binary pods during the rolling deploy (which did
-- not fold), with no residual window.
--
-- Everything here is idempotent and safe against live traffic: heals are
-- guarded on the observed stale value, so a checkpoint a writer folds
-- concurrently is skipped rather than clobbered. If a run aborts on a
-- deadlock with a concurrent flush, or you are in any doubt, just run it
-- again - a clean ledger makes every statement a no-op.
--
-- Keep this script around: re-running step 1 is also the way to reconcile
-- balances after any manual surgery on credits_transactions.
--
-- HOW TO RUN: interactively in psql, statement by statement - NOT as a
-- single "psql -f" pass. Step 2's batch statement must be repeated until it
-- returns 0, its index created once before the loop and dropped once after
-- (CREATE INDEX CONCURRENTLY also cannot run inside a transaction block).
--
-- Staging dry run (2026-07-09, 45.6M-row / 36 GB ledger on Neon): step 1
-- healed 2870 checkpoints in about 2 minutes; step 2 drained 2.87M rows in
-- about 30 minutes including the index build.

-- 1. REQUIRED: heal every checkpoint whose balance disagrees with the
--    ledger. Single pass over the ledger, point updates on drifted
--    checkpoints only.
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

-- 2. OPTIONAL: drain the historical batch backlog into batch_aggregates so
--    pre-deploy batches show as grouped entries on the transactions page.
--
-- 2a. Partial index so each batch below finds its rows instantly instead of
--     seq-scanning the whole table per batch. Rows leave the index as they
--     are marked, so it shrinks to empty. If a previous CONCURRENTLY attempt
--     was interrupted the index exists but is INVALID (check pg_index.indisvalid)
--     - drop it and recreate.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_credits_tx_backfill_pending
    ON credits_transactions (id)
    WHERE fusillade_batch_id IS NOT NULL AND is_aggregated = false;

-- 2b. Aggregate in batches of 100k so each transaction stays short (bounded
--     locks and WAL, visible progress). RE-RUN until it returns 0.
WITH pending AS (
    SELECT id
    FROM credits_transactions
    WHERE fusillade_batch_id IS NOT NULL
      AND is_aggregated = false
    LIMIT 100000
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

-- 2c. Once 2b reports 0, drop the scaffolding index.
DROP INDEX CONCURRENTLY IF EXISTS idx_credits_tx_backfill_pending;
