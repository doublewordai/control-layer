-- Recreate request_update_notify trigger

-- Create the trigger function
CREATE OR REPLACE FUNCTION notify_request_update()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        'request_updates',
        json_build_object(
            'id', NEW.id::text,
            'state', NEW.state::text,
            'updated_at', NEW.updated_at::text
        )::text
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Create the trigger
CREATE TRIGGER request_update_notify
    AFTER INSERT OR UPDATE ON requests
    FOR EACH ROW
    EXECUTE FUNCTION notify_request_update();
