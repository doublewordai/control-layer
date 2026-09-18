-- One row per payment instrument (Stripe fingerprint of a card or bank
-- account) that has been used to claim signup verification credits, and the
-- account that claimed it. The same card or IBAN produces the same
-- fingerprint on every Stripe customer, so this is what stops one instrument
-- from funding many accounts: in September 2026, 178 accounts verified with a
-- single SEPA IBAN and each collected the credit.
--
-- Verification itself (users.verified) is not gated by this table; only the
-- credit is. No foreign key to users: rows must survive the user being
-- soft-deleted, or a scrubbed-and-recreated account could claim again.
CREATE TABLE verification_instruments (
    fingerprint TEXT PRIMARY KEY,
    user_id     UUID NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
