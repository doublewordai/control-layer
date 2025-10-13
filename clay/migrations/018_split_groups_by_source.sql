-- 1. Add the column as nullable (no default)
ALTER TABLE groups
    ADD COLUMN source TEXT;

-- 2. Set existing rows to 'native'
UPDATE groups
SET source = 'native';

-- 3. Make the column NOT NULL (optional, if you want to enforce it later)
ALTER TABLE groups
    ALTER COLUMN source SET NOT NULL;
