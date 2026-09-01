-- Remove denormalized model column from requests table
DROP INDEX IF EXISTS idx_requests_model;

ALTER TABLE requests
    DROP COLUMN model;
