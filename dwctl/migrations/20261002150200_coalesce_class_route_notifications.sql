-- The shared notifier uses clock_timestamp(), yielding a different payload per
-- row. Use a transaction-stable payload for this new table so bulk catalog writes
-- coalesce to one notification while zero-row writes still emit nothing.
CREATE FUNCTION notify_class_route_change() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || (extract(epoch from transaction_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER model_serving_classes_config_change ON model_serving_classes;
CREATE TRIGGER model_serving_classes_config_change
    AFTER INSERT OR UPDATE OR DELETE ON model_serving_classes
    FOR EACH ROW EXECUTE FUNCTION notify_class_route_change();
