-- Add a `verified` flag to users for tier-based rate limiting in onwards.
--
-- The flag is set true when real money moves: at successful Stripe checkout
-- (process_payment_session) and successful auto-topup charge (charge_auto_topup).
-- The onwards sync reads this flag when building per-key rate limits, picking
-- the verified or unverified tier from config when the api_key has no explicit
-- per-key override.
--
-- For organizations (users.user_type = 'organization'), this column flips
-- naturally because both payment paths act on the resolved billing target,
-- which is the org when an org is paying. Org keys are tiered by the org's
-- verified status; personal keys by the individual's.
ALTER TABLE users
    ADD COLUMN verified BOOLEAN NOT NULL DEFAULT false;

-- Backfill: any user who has ever had a successful real-money purchase
-- (transaction_type = 'purchase', positive amount). Admin grants do not count;
-- those are credits issued without a payment.
UPDATE users u
SET verified = true
WHERE EXISTS (
    SELECT 1 FROM credits_transactions ct
    WHERE ct.user_id = u.id
      AND ct.transaction_type = 'purchase'
      AND ct.amount > 0
);

-- Notify onwards sync when verified flips so the in-memory cache picks up the
-- new tier without waiting for an unrelated config change. Scoped to UPDATEs
-- of this one column (via the WHEN clause) so unrelated user edits do not
-- trigger a sync rebuild.
CREATE TRIGGER users_verified_notify
    AFTER UPDATE OF verified ON users
    FOR EACH ROW
    WHEN (OLD.verified IS DISTINCT FROM NEW.verified)
    EXECUTE FUNCTION notify_config_change();
