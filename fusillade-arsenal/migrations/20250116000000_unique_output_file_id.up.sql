-- Add unique constraints to output_file_id and error_file_id
-- Each batch should have unique output and error files

ALTER TABLE batches ADD CONSTRAINT batches_output_file_id_key UNIQUE (output_file_id);
ALTER TABLE batches ADD CONSTRAINT batches_error_file_id_key UNIQUE (error_file_id);
