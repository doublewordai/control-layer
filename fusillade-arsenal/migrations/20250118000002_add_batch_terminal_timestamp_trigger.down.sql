-- Remove batch terminal timestamp trigger and function

DROP TRIGGER IF EXISTS update_batch_terminal_timestamps_trigger ON requests;
DROP FUNCTION IF EXISTS update_batch_terminal_timestamps();
