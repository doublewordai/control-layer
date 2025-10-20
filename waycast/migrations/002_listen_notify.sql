-- Add LISTEN/NOTIFY triggers for real-time updates

CREATE OR REPLACE FUNCTION notify_config_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed', '');
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER deployed_models_notify
    AFTER INSERT OR UPDATE OR DELETE ON deployed_models
    EXECUTE FUNCTION notify_config_change();

CREATE TRIGGER user_groups_notify
    AFTER INSERT OR DELETE ON user_groups
    EXECUTE FUNCTION notify_config_change();

CREATE TRIGGER deployment_groups_notify
    AFTER INSERT OR DELETE ON deployment_groups
    EXECUTE FUNCTION notify_config_change();

CREATE TRIGGER api_keys_notify
    AFTER INSERT OR UPDATE OR DELETE ON api_keys
    EXECUTE FUNCTION notify_config_change();