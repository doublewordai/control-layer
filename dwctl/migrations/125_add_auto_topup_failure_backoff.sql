-- Auto top-up retry backoff.
--
-- A failed charge writes no credits_transactions row, so the source_id
-- idempotency check passes again on the very next poller tick. Without this
-- state a declined card is retried every tick (30s) forever, emailing the
-- customer each time.
--
-- failure_count drives exponential backoff and doubles as the "already
-- emailed" marker: reaching the configured threshold means we have stopped
-- retrying and sent exactly one failure email. Both columns reset on a
-- successful charge or when the user reconfigures auto top-up.
ALTER TABLE users ADD COLUMN IF NOT EXISTS auto_topup_failure_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN IF NOT EXISTS auto_topup_failed_at TIMESTAMPTZ DEFAULT NULL;
