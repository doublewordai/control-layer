-- Add LISTEN/NOTIFY triggers for webhook event processing
-- Platform events (user.created, api_key.created) are detected via PG triggers
-- and processed by the notification poller to create webhook deliveries.

CREATE OR REPLACE FUNCTION notify_webhook_event() RETURNS trigger AS $$
BEGIN
    -- Payload format: "table_name:record_id"
    PERFORM pg_notify('webhook_event', TG_TABLE_NAME || ':' || NEW.id::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER users_webhook_notify
    AFTER INSERT ON users
    FOR EACH ROW
    EXECUTE FUNCTION notify_webhook_event();

CREATE TRIGGER api_keys_webhook_notify
    AFTER INSERT ON api_keys
    FOR EACH ROW
    EXECUTE FUNCTION notify_webhook_event();

-- Safety net: prevent duplicate webhook deliveries for the same event.
-- The notification poller runs on the elected leader only, but this index
-- ensures correctness even during leadership transitions.
CREATE UNIQUE INDEX idx_webhook_deliveries_unique_event
    ON webhook_deliveries (webhook_id, event_type, resource_id)
    WHERE resource_id IS NOT NULL;
