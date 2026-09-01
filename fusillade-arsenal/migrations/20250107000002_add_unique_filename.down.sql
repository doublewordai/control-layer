-- Remove UNIQUE constraint from files.name
ALTER TABLE files
DROP CONSTRAINT files_name_unique;
