-- Convert inference_endpoints ID from integer to UUID
-- This is a simpler approach that minimizes changes and risk

-- Step 1: Add UUID column
ALTER TABLE inference_endpoints ADD COLUMN uuid_id UUID DEFAULT gen_random_uuid() NOT NULL;

-- Step 2: Add UUID column to deployed_models  
ALTER TABLE deployed_models ADD COLUMN hosted_on_uuid UUID;

-- Step 3: Create mapping from integer to UUID
UPDATE deployed_models dm
SET hosted_on_uuid = ie.uuid_id
FROM inference_endpoints ie  
WHERE dm.hosted_on = ie.id;

-- Step 4: Drop old foreign key
ALTER TABLE deployed_models DROP CONSTRAINT deployed_models_hosted_on_fkey;

-- Step 5: Set new UUID columns as primary/foreign keys
ALTER TABLE inference_endpoints DROP CONSTRAINT inference_endpoints_pkey;
ALTER TABLE inference_endpoints ADD PRIMARY KEY (uuid_id);

-- Step 6: Drop old integer columns
ALTER TABLE deployed_models DROP COLUMN hosted_on;
ALTER TABLE inference_endpoints DROP COLUMN id;

-- Step 7: Rename UUID columns to standard names
ALTER TABLE deployed_models RENAME COLUMN hosted_on_uuid TO hosted_on;
ALTER TABLE inference_endpoints RENAME COLUMN uuid_id TO id;

-- Step 8: Add back foreign key constraint
ALTER TABLE deployed_models
ADD CONSTRAINT deployed_models_hosted_on_fkey 
FOREIGN KEY (hosted_on) REFERENCES inference_endpoints(id) ON DELETE CASCADE;

-- Step 9: Ensure hosted_on is NOT NULL
ALTER TABLE deployed_models ALTER COLUMN hosted_on SET NOT NULL;

-- Step 10: Add index for performance
CREATE INDEX idx_deployed_models_hosted_on ON deployed_models(hosted_on);