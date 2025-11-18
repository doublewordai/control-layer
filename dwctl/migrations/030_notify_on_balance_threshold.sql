-- Add trigger to notify onwards when user balance crosses the zero threshold
-- This ensures API keys are removed when balance goes negative and restored when it goes positive

CREATE OR REPLACE FUNCTION notify_on_balance_threshold_crossing() RETURNS trigger AS $$
DECLARE
    old_balance DECIMAL(12, 8);
BEGIN
    -- Get the previous balance for this user
    -- For the first transaction, old_balance will be NULL (treated as 0)
    SELECT balance_after INTO old_balance
    FROM credits_transactions
    WHERE id = NEW.previous_transaction_id;

    -- If old_balance is NULL, this is the first transaction, treat as 0
    old_balance := COALESCE(old_balance, 0);

    -- Check if we crossed the zero threshold in either direction
    IF (old_balance > 0 AND NEW.balance_after <= 0) OR
       (old_balance <= 0 AND NEW.balance_after > 0) THEN
        -- Balance crossed zero threshold, notify onwards to reload config
        PERFORM pg_notify('auth_config_changed',
            json_build_object(
                'user_id', NEW.user_id,
                'old_balance', old_balance,
                'new_balance', NEW.balance_after,
                'threshold_crossed', 'zero'
            )::text
        );
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER credits_balance_threshold_notify
    AFTER INSERT ON credits_transactions
    FOR EACH ROW
    EXECUTE FUNCTION notify_on_balance_threshold_crossing();
