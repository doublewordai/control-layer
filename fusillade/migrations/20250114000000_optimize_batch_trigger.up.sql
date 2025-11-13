-- Optimize batch status trigger to only run on UPDATE (state changes)
-- INSERT: Batch creation manually sets counters after bulk insert
-- DELETE: Cascade deletes the batch anyway, no need to update counters
-- UPDATE: Real-time counter updates as daemons process requests

-- Drop the existing trigger
DROP TRIGGER IF EXISTS update_batch_status_on_request_change ON requests;

-- Recreate trigger for UPDATE only, with optimization to only fire on state changes
CREATE TRIGGER update_batch_status_on_request_change
    AFTER UPDATE ON requests
    FOR EACH ROW
    WHEN (OLD.state IS DISTINCT FROM NEW.state)
    EXECUTE FUNCTION update_batch_on_request_change();
