-- Restore original trigger that fires on INSERT, UPDATE, and DELETE

DROP TRIGGER IF EXISTS update_batch_status_on_request_change ON requests;

CREATE TRIGGER update_batch_status_on_request_change
    AFTER INSERT OR UPDATE OR DELETE ON requests
    FOR EACH ROW
    EXECUTE FUNCTION update_batch_on_request_change();
