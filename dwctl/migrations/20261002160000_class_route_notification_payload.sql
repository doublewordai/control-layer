-- A transaction-start timestamp is not a notification-send timestamp. Keep the
-- payload identical within the transaction for coalescing, without reporting
-- transaction age as cache propagation lag. The listener also accepts bare names.
CREATE OR REPLACE FUNCTION notify_class_route_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed', TG_TABLE_NAME);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
