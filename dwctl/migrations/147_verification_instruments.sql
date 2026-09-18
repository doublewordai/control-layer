-- One row per payment instrument (Stripe fingerprint of a card or bank
-- account) that has verified an account, and that account. The same card or
-- IBAN produces the same fingerprint on every Stripe customer, so this is
-- what makes one instrument verify one account: in September 2026, 178
-- accounts verified with a single SEPA IBAN and each collected the signup
-- credit.
--
-- Gates both users.verified and the verification credit: a setup session
-- whose instrument is held by another account verifies nothing and pays
-- nothing (the instrument is still saved and may pay for top-ups). Only
-- setup sessions consult this table; payment-mode Checkout never does.
--
-- Enforcement is forward-only. The table starts empty, so an instrument that
-- verified an account before this migration is unclaimed and may verify one
-- more account after it; the first post-rollout verification claims it.
-- Backfilling from Stripe (successful SetupIntents -> payment method
-- fingerprint -> customer -> user) is possible but deliberately not done
-- here, so the migration stays free of external calls and production data.
--
-- Scope is one regional plane. Each plane has its own database while one
-- Stripe account serves all of them, so an instrument can verify one account
-- per plane. Closing that would need a cross-plane store; Stripe offers no
-- per-fingerprint metadata to hang a claim on.
--
-- No foreign key to users: rows must survive the user being soft-deleted,
-- or a scrubbed-and-recreated account could claim again.
CREATE TABLE verification_instruments (
    fingerprint TEXT PRIMARY KEY,
    user_id     UUID NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
