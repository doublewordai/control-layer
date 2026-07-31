ALTER TABLE users
    ADD COLUMN auto_topup_soft_failure_count INTEGER NOT NULL DEFAULT 0
        CHECK (auto_topup_soft_failure_count >= 0),
    ADD COLUMN auto_topup_retry_after TIMESTAMPTZ;
