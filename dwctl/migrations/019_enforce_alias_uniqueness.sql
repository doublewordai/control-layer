-- Enforce uniqueness on deployed_models.alias with backwards compatibility
-- This migration handles existing duplicates by appending suffixes

-- Step 1: Create a function to generate unique aliases for duplicates
CREATE OR REPLACE FUNCTION generate_unique_alias(base_alias TEXT, existing_id UUID DEFAULT NULL)
RETURNS TEXT AS $$
DECLARE
    counter INTEGER := 1;
    new_alias TEXT;
    exists_check BOOLEAN;
BEGIN
    -- Start with the base alias
    new_alias := base_alias;
    
    -- Check if this alias already exists (excluding the current record if updating)
    LOOP
        SELECT EXISTS(
            SELECT 1 FROM deployed_models 
            WHERE alias = new_alias 
            AND (existing_id IS NULL OR id != existing_id)
        ) INTO exists_check;
        
        -- If no conflict, return this alias
        IF NOT exists_check THEN
            RETURN new_alias;
        END IF;
        
        -- Generate next candidate with suffix
        counter := counter + 1;
        new_alias := base_alias || ' (' || counter || ')';
    END LOOP;
END;
$$ LANGUAGE plpgsql;

-- Step 2: Handle existing duplicates by updating them with unique suffixes
-- We'll update duplicates in order of creation (older records keep original name)
DO $$
DECLARE
    dup_record RECORD;
    new_unique_alias TEXT;
BEGIN
    -- Find all aliases that have duplicates
    FOR dup_record IN 
        SELECT alias, array_agg(id ORDER BY created_at) as ids
        FROM deployed_models 
        GROUP BY alias 
        HAVING COUNT(*) > 1
    LOOP
        -- For each set of duplicates, update all but the first (oldest) record
        FOR i IN 2..array_length(dup_record.ids, 1) LOOP
            -- Generate a unique alias for this duplicate
            new_unique_alias := generate_unique_alias(dup_record.alias, dup_record.ids[i]);
            
            -- Update the duplicate record
            UPDATE deployed_models 
            SET alias = new_unique_alias,
                updated_at = NOW()
            WHERE id = dup_record.ids[i];
            
            -- Log the change for debugging
            RAISE NOTICE 'Updated duplicate alias: % -> % for deployment %', 
                dup_record.alias, new_unique_alias, dup_record.ids[i];
        END LOOP;
    END LOOP;
END $$;

-- Step 3: Add the unique constraint now that duplicates are resolved
ALTER TABLE deployed_models 
ADD CONSTRAINT deployed_models_alias_unique UNIQUE (alias);

-- Step 4: Add an index for performance
-- The unique constraint already creates an index, so this is just for documentation
COMMENT ON CONSTRAINT deployed_models_alias_unique ON deployed_models IS 
'Ensures model aliases are unique across all deployments for proper routing';

-- Step 5: Clean up the helper function
DROP FUNCTION generate_unique_alias(TEXT, UUID);