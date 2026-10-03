-- Nullable snapshots preserve legacy analytics and mixed-version writers.
-- No FK: deleting a route must not delete or reinterpret accepted requests.
SET LOCAL lock_timeout = '5s';

ALTER TABLE http_analytics
    ADD COLUMN canonical_model_id UUID,
    ADD COLUMN serving_class_id UUID,
    ADD COLUMN destination_endpoint_id UUID,
    ADD COLUMN upstream_model_name TEXT,
    ADD COLUMN submitted_model TEXT;
COMMENT ON COLUMN http_analytics.canonical_model_id IS
    'Class-route identity captured at dispatch; NULL retains legacy price resolution after activation.';
COMMENT ON COLUMN http_analytics.upstream_model_name IS
    'Effective destination name, including legacy routes; scoped by endpoint/served_by.';
-- Synonyms remain startup-loaded; class destinations must stay reactive.
CREATE TRIGGER model_serving_classes_config_change
    AFTER INSERT OR UPDATE OR DELETE ON model_serving_classes
    FOR EACH STATEMENT EXECUTE FUNCTION notify_config_change();
