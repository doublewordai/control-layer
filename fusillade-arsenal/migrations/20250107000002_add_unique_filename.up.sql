-- Add UNIQUE constraint to files.name to prevent duplicate filenames
ALTER TABLE files
ADD CONSTRAINT files_name_unique UNIQUE (name);
