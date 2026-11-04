-- DW-1005: the onwards sync now reads secret_hash instead of secret, so the
-- scoped api_keys UPDATE-notify (migrations 101 and 122) fires on secret_hash
-- changes instead of secret. Every key rotation writes both columns, so this
-- still fires on rotation; nulling the plaintext in a later release will not
-- trigger a reload storm. Otherwise identical to the definition in 122.
CREATE OR REPLACE FUNCTION notify_api_keys_config_change() RETURNS trigger AS $$
DECLARE
    relevant_change boolean := false;
BEGIN
    IF TG_OP = 'INSERT' THEN
        relevant_change := EXISTS (SELECT 1 FROM new_rows);
    ELSIF TG_OP = 'DELETE' THEN
        relevant_change := EXISTS (SELECT 1 FROM old_rows);
    ELSIF TG_OP = 'UPDATE' THEN
        -- Only columns the sync query reads matter; metadata-only updates
        -- (name, description, last_used) must not reload the cache. Joined on the
        -- immutable primary key.
        relevant_change := EXISTS (
            SELECT 1
            FROM new_rows n
            JOIN old_rows o ON o.id = n.id
            WHERE o.secret_hash          IS DISTINCT FROM n.secret_hash
               OR o.purpose              IS DISTINCT FROM n.purpose
               OR o.user_id              IS DISTINCT FROM n.user_id
               OR o.requests_per_second  IS DISTINCT FROM n.requests_per_second
               OR o.burst_size           IS DISTINCT FROM n.burst_size
               OR o.is_deleted           IS DISTINCT FROM n.is_deleted
               OR o.hidden               IS DISTINCT FROM n.hidden
               OR o.spend_limit          IS DISTINCT FROM n.spend_limit
               OR o.spend_limit_interval IS DISTINCT FROM n.spend_limit_interval
               OR o.parent_api_key_id    IS DISTINCT FROM n.parent_api_key_id
        );
    END IF;

    IF relevant_change THEN
        -- Match notify_config_change()'s payload format (migration 049) so the
        -- cache-sync lag metric keeps attributing reloads to the api_keys table.
        PERFORM pg_notify('auth_config_changed',
            'api_keys:' || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
