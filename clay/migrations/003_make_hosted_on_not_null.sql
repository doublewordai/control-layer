-- Make hosted_on field NOT NULL since all models must have an endpoint

-- First, update any existing models that have NULL hosted_on to use the default endpoint (id=1)
UPDATE deployed_models 
SET hosted_on = 1 
WHERE hosted_on IS NULL;

-- Then add the NOT NULL constraint
ALTER TABLE deployed_models 
ALTER COLUMN hosted_on SET NOT NULL;