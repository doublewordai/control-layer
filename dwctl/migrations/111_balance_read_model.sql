-- Balance read model: make user_balance_checkpoints a total read model.
--
-- Before this migration, user_balance_checkpoints was an opportunistic cache
-- (refreshed 1-in-1000 writes), so zero-transaction and low-volume users had
-- no row and every consumer had to re-aggregate ledger history at read time.
-- After this migration every user has a checkpoint row from creation
-- (trigger below), so consumers can treat the table as total and read
-- balances as point reads. Writers keep it current synchronously: the
-- analytics batcher folds one grouped update per user per flush, and
-- create_transaction folds per row, both atomically with the ledger insert.
--
-- This migration is deliberately instant: dwctl applies migrations at pod
-- startup, so anything slow here turns into a k8s crash loop that also
-- blocks writers while it holds locks. The ledger-sized work is the one-off
-- post-deploy script scripts/backfill_balance_checkpoints.sql, run manually
-- once all pods are on this release: it baselines every checkpoint from the
-- ledger and aggregates the historical batch backlog. Until it runs,
-- existing rows keep their pre-migration cached values - the same staleness
-- they had before this deploy. There is no background job.

-- 1. Zero checkpoint for every user with no row yet (the bot-signup
--    population): the read model is total from here on.
INSERT INTO user_balance_checkpoints (user_id, checkpoint_seq, balance)
SELECT id, 0, 0
FROM users
ON CONFLICT (user_id) DO NOTHING;

-- 2. Keep it total: create the checkpoint row together with the user,
--    whichever code path inserts the user. This also covers user creation by
--    old binaries during the rolling deploy that ships this migration.
CREATE OR REPLACE FUNCTION create_user_balance_checkpoint() RETURNS trigger AS $$
BEGIN
    INSERT INTO user_balance_checkpoints (user_id, checkpoint_seq, balance)
    VALUES (NEW.id, 0, 0)
    ON CONFLICT (user_id) DO NOTHING;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER users_create_balance_checkpoint
    AFTER INSERT ON users
    FOR EACH ROW
    EXECUTE FUNCTION create_user_balance_checkpoint();
