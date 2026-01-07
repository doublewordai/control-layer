-- Add timestamp to NOTIFY payload for cache sync lag measurement
-- This allows measuring the time between DB change and cache update

CREATE OR REPLACE FUNCTION notify_config_change() RETURNS trigger AS $$
BEGIN
    -- Include microsecond-precision timestamp and table name in payload
    -- Format: "table_name:epoch_microseconds"
    PERFORM pg_notify('auth_config_changed',
        TG_TABLE_NAME || ':' || (extract(epoch from clock_timestamp()) * 1000000)::bigint::text);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
