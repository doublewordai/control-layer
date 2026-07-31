-- Lifetime request allowances for newly-created users' hidden UI execution
-- keys. There is deliberately no backfill: absence of a row means the key has
-- no trial allowance, while a row with remaining_requests = 0 records an
-- exhausted trial and stays routable long enough for its final accepted work.
CREATE TABLE api_key_request_allowances (
    api_key_id UUID PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
    initial_requests BIGINT NOT NULL CHECK (initial_requests > 0),
    remaining_requests BIGINT NOT NULL CHECK (
        remaining_requests >= 0 AND remaining_requests <= initial_requests
    ),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

COMMENT ON TABLE api_key_request_allowances IS
    'Lifetime UI-only request allowances attached to hidden playground and batch API keys';
COMMENT ON COLUMN api_key_request_allowances.remaining_requests IS
    'Requests not yet reserved; zero rows remain so already-accepted work stays routable';

-- Reservation decrements intentionally do not notify onwards. Deletion means
-- the first positive credit gain permanently revoked the allowance and must
-- refresh routing even when the balance did not cross zero.
CREATE OR REPLACE FUNCTION notify_request_allowance_delete() RETURNS trigger AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM old_rows) THEN
        PERFORM pg_notify(
            'auth_config_changed',
            'api_key_request_allowances:' ||
                (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text
        );
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER api_key_request_allowances_notify_delete
    AFTER DELETE ON api_key_request_allowances
    REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION notify_request_allowance_delete();
