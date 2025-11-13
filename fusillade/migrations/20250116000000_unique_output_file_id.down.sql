-- Remove unique constraints from output_file_id and error_file_id

ALTER TABLE batches DROP CONSTRAINT batches_output_file_id_key;
ALTER TABLE batches DROP CONSTRAINT batches_error_file_id_key;
