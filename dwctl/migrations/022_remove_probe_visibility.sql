-- Remove public visibility flag from probes

-- Drop the index first
DROP INDEX IF EXISTS idx_probes_public;

-- Drop the public column
ALTER TABLE probes DROP COLUMN IF EXISTS public;
