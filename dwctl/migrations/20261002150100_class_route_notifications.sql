-- ON CONFLICT ... WHERE unchanged produces no row events, so catalog restarts
-- do not manufacture config reloads. A transaction still coalesces notifications.
DROP TRIGGER model_serving_classes_config_change ON model_serving_classes;
CREATE TRIGGER model_serving_classes_config_change
    AFTER INSERT OR UPDATE OR DELETE ON model_serving_classes
    FOR EACH ROW EXECUTE FUNCTION notify_config_change();
