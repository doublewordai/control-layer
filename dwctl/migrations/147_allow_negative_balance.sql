-- Contracted accounts may accrue debt while usage charges continue normally.
-- Database-only policy on the billing account (individual or organization).
SET LOCAL lock_timeout = '5s';

ALTER TABLE users ADD COLUMN allow_negative_balance BOOLEAN NOT NULL DEFAULT false;

COMMENT ON COLUMN users.allow_negative_balance IS
    'Bypass credit balance admission checks while continuing to record usage charges. Set manually for contracted accounts.';

CREATE TRIGGER users_allow_negative_balance_notify
    AFTER UPDATE OF allow_negative_balance ON users
    FOR EACH ROW
    WHEN (OLD.allow_negative_balance IS DISTINCT FROM NEW.allow_negative_balance)
    EXECUTE FUNCTION notify_config_change();
